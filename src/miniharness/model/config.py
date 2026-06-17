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
class ModelRuntimeConfig:
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
    def from_env(cls) -> "ModelRuntimeConfig":
        providers: dict[str, dict[str, Any]] = {}
        if os.getenv("MINIHARNESS_BASE_URL") and os.getenv("MINIHARNESS_MODEL"):
            providers["openai_compatible"] = {
                "base_url": os.environ["MINIHARNESS_BASE_URL"],
                "api_key": os.getenv("MINIHARNESS_API_KEY", ""),
                "model": os.environ["MINIHARNESS_MODEL"],
            }
        return cls(
            default_provider=os.getenv("MINIHARNESS_MODEL_PROVIDER", "openai_compatible"),
            default_model=os.getenv("MINIHARNESS_MODEL"),
            providers=providers,
        )

