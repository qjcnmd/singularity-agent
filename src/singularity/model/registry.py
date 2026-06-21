from __future__ import annotations

from singularity.model.errors import ModelCapabilityError, ModelProviderNotFound
from singularity.model.models import ModelPreferences
from singularity.model.providers import ModelProvider


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

    @staticmethod
    def provider_capability_summary(provider: ModelProvider) -> dict[str, object]:
        capabilities = provider.capabilities()
        return {
            "provider": provider.name(),
            "supports_tools": capabilities.supports_tools,
            "supports_parallel_tool_calls": capabilities.supports_parallel_tool_calls,
            "supports_streaming": capabilities.supports_streaming,
            "supports_json_mode": capabilities.supports_json_mode,
            "supports_system_message": capabilities.supports_system_message,
            "supports_developer_message": capabilities.supports_developer_message,
            "max_context_tokens": capabilities.max_context_tokens,
            "max_output_tokens": capabilities.max_output_tokens,
        }
