from __future__ import annotations

from miniharness.model.errors import ModelCapabilityError, ModelProviderNotFound
from miniharness.model.models import ModelPreferences
from miniharness.model.providers import ModelProvider


class ModelProviderRegistry:
    def __init__(self, *, default_provider_name: str | None = None) -> None:
        self.default_provider_name = default_provider_name
        self._providers: dict[str, ModelProvider] = {}

    def register(self, provider: ModelProvider) -> None:
        name = provider.name()
        self._providers[name] = provider
        if self.default_provider_name is None:
            self.default_provider_name = name

    def get(self, name: str) -> ModelProvider:
        try:
            return self._providers[name]
        except KeyError as exc:
            raise ModelProviderNotFound(f"Unknown model provider: {name}") from exc

    def list(self) -> list[ModelProvider]:
        return list(self._providers.values())

    def default_provider(self) -> ModelProvider:
        if not self.default_provider_name:
            raise ModelProviderNotFound("No default model provider is configured.")
        return self.get(self.default_provider_name)

    def select_provider(
        self,
        preferences: ModelPreferences,
        *,
        purpose: object | None = None,
    ) -> ModelProvider:
        del purpose
        if preferences.provider_name:
            return self.get(preferences.provider_name)
        return self.default_provider()

    def check_capabilities(
        self,
        provider: ModelProvider,
        *,
        requires_tools: bool = False,
        requires_streaming: bool = False,
        requires_json_mode: bool = False,
    ) -> None:
        capabilities = provider.capabilities()
        if requires_tools and not capabilities.supports_tools:
            raise ModelCapabilityError(
                f"Provider {provider.name()} does not support tool calls."
            )
        if requires_streaming and not capabilities.supports_streaming:
            raise ModelCapabilityError(
                f"Provider {provider.name()} does not support streaming."
            )
        if requires_json_mode and not capabilities.supports_json_mode:
            raise ModelCapabilityError(
                f"Provider {provider.name()} does not support JSON mode."
            )
