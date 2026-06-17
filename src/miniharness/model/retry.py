from __future__ import annotations

import time
from dataclasses import dataclass, field
from typing import Any, Callable

from miniharness.model.errors import ModelRetryExhausted
from miniharness.model.models import ModelError, ModelErrorKind


@dataclass
class RetryPolicy:
    max_attempts: int = 3
    backoff_seconds: float = 0.25
    fallback_models: list[str] = field(default_factory=list)


class ModelRetryController:
    RETRYABLE = {
        ModelErrorKind.NETWORK_ERROR,
        ModelErrorKind.TIMEOUT,
        ModelErrorKind.RATE_LIMITED,
        ModelErrorKind.PROVIDER_OVERLOADED,
    }

    def __init__(self, policy: RetryPolicy) -> None:
        self.policy = policy
        self.retry_count = 0
        self.fallback_count = 0

    def should_retry(self, error: ModelError, *, attempt: int) -> bool:
        return (
            attempt < self.policy.max_attempts
            and error.kind in self.RETRYABLE
            and error.retryable
        )

    def run(self, operation: Callable[[str | None], Any], *, initial_model: str | None = None) -> Any:
        attempt = 1
        model_name = initial_model
        fallback_index = 0
        last_error: ModelError | None = None
        while attempt <= self.policy.max_attempts:
            try:
                return operation(model_name)
            except ModelError as exc:
                last_error = exc
                if not self.should_retry(exc, attempt=attempt):
                    raise
                self.retry_count += 1
                if fallback_index < len(self.policy.fallback_models):
                    model_name = self.policy.fallback_models[fallback_index]
                    fallback_index += 1
                    self.fallback_count += 1
                if self.policy.backoff_seconds:
                    time.sleep(self.policy.backoff_seconds * attempt)
                attempt += 1
        raise ModelRetryExhausted(str(last_error.message if last_error else "Retry exhausted."))

