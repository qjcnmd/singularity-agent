from singularity.model import (
    ModelError,
    ModelErrorKind,
    ModelRetryController,
    RetryPolicy,
)


def test_retry_controller_retries_retryable_errors_and_uses_fallback() -> None:
    controller = ModelRetryController(
        RetryPolicy(max_attempts=3, backoff_seconds=0, fallback_models=["fallback-model"])
    )
    calls = {"count": 0}

    def operation(model_name: str | None) -> str:
        calls["count"] += 1
        if calls["count"] == 1:
            raise ModelError(
                kind=ModelErrorKind.NETWORK_ERROR,
                message="network",
                retryable=True,
                model_name=model_name,
            )
        return model_name or "primary"

    assert controller.run(operation, initial_model="primary") == "fallback-model"
    assert calls["count"] == 2
    assert controller.retry_count == 1
    assert controller.fallback_count == 1

    no_retry = ModelRetryController(RetryPolicy(max_attempts=3, backoff_seconds=0))
    assert not no_retry.should_retry(
        ModelError(kind=ModelErrorKind.AUTH_ERROR, message="auth", retryable=False),
        attempt=1,
    )


def test_retry_controller_uses_exponential_backoff_with_jitter(monkeypatch) -> None:
    sleeps: list[float] = []
    monkeypatch.setattr("singularity.model.retry.time.sleep", sleeps.append)
    monkeypatch.setattr("singularity.model.retry.random.uniform", lambda _low, _high: 0.05)
    controller = ModelRetryController(
        RetryPolicy(max_attempts=3, backoff_seconds=0.5, jitter_ratio=0.2)
    )
    calls = {"count": 0}

    def operation(model_name: str | None) -> str:
        del model_name
        calls["count"] += 1
        if calls["count"] < 3:
            raise ModelError(
                kind=ModelErrorKind.TIMEOUT,
                message="timeout",
                retryable=True,
            )
        return "ok"

    assert controller.run(operation) == "ok"
    assert sleeps == [0.55, 1.05]
    assert controller.retry_count == 2

