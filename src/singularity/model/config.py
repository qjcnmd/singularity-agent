from __future__ import annotations

import os
from dataclasses import dataclass, field
from typing import Any


@dataclass
class ContextExportPolicy:
    allow_workspace_source: bool = True
    deny_secret_like_content: bool = True
    deny_env_content: bool = True
    redact_before_send: bool = True
    local_only_sensitive_context: bool = True


@dataclass
class ModelRunnerConfig:
    default_provider: str = "openai_compatible"
    default_model: str | None = None
    providers: dict[str, dict[str, Any]] = field(default_factory=dict)
    request_timeout_seconds: float = 60.0
    default_max_output_tokens: int = 4096
    default_temperature: float | None = None
    enable_streaming: bool = True
    store_raw_responses: bool = False
    redact_prompts_in_trace: bool = True
    allow_remote_provider: bool = True
    context_export_policy: ContextExportPolicy = field(default_factory=ContextExportPolicy)
    retry_policy: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_env(
        cls,
        *,
        base_url: str | None = None,
        model: str | None = None,
        store_raw_responses: bool | None = None,
    ) -> "ModelRunnerConfig":
        providers: dict[str, dict[str, Any]] = {}
        resolved_base_url = base_url or os.getenv("SINGULARITY_BASE_URL")
        resolved_model = model or os.getenv("SINGULARITY_MODEL")
        if resolved_base_url and resolved_model:
            providers["openai_compatible"] = {
                "base_url": resolved_base_url,
                "api_key": os.getenv("SINGULARITY_API_KEY", ""),
                "model": resolved_model,
            }
        return cls(
            default_provider=os.getenv("SINGULARITY_MODEL_PROVIDER", "openai_compatible"),
            default_model=resolved_model,
            providers=providers,
            store_raw_responses=bool(store_raw_responses),
        )

