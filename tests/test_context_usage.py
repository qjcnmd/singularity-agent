from typing import Any

from singularity.context.usage import ContextUsageReporter


class _Store:
    pass


def _provider_name(provider: Any) -> str | None:
    reporter = ContextUsageReporter(
        run_id="run_1",
        store=_Store(),
        provider=provider,
        model_runner=None,
    )
    return reporter.current_cache_attribution().provider_name


def test_cache_attribution_accepts_string_provider_name_attribute() -> None:
    class Provider:
        name = "openai-compatible"

    assert _provider_name(Provider()) == "openai-compatible"


def test_cache_attribution_accepts_callable_provider_name() -> None:
    class Provider:
        def name(self) -> str:
            return "callable-provider"

    assert _provider_name(Provider()) == "callable-provider"


def test_cache_attribution_prefers_provider_name_attribute() -> None:
    class Provider:
        provider_name = "canonical-provider"
        name = "fallback-provider"

    assert _provider_name(Provider()) == "canonical-provider"
