from __future__ import annotations

from typing import Any

from singularity.tools.cache import ToolResultCache
from singularity.tools.execution_pipeline import ToolExecutionPipelineState
from singularity.tools.models import ToolResult


class ToolExecutionCache:
    def __init__(self, executor: Any, cache: ToolResultCache) -> None:
        self.executor = executor
        self.cache = cache

    def precheck(self, state: ToolExecutionPipelineState) -> ToolResult | None:
        assert state.spec is not None
        assert state.validated_args is not None
        self.executor._throw_if_cancelled()
        state.cache_key = self.executor._cache_key(state.spec, state.validated_args)
        cache_policy = state.spec.cache_policy
        if self.executor._should_cache(state.spec):
            cached = self.cache.get(
                state.cache_key,
                ttl_seconds=cache_policy.ttl_seconds if cache_policy else None,
            )
            if cached is not None:
                state.cache_hit = True
                cached.metadata["cache_hit"] = True
                if state.approval_grant_id:
                    cached.metadata["approval_grant_id"] = state.approval_grant_id
                if state.policy_decision_id:
                    cached.metadata["policy_decision_id"] = state.policy_decision_id
                state.output_digest = cached.metadata.get("output_digest") or self.executor._result_digest(cached)
                state.remember_replay = True
                return cached

        delegated_error = self.executor._delegated_backend_error(state.spec)
        if delegated_error is not None:
            state.remember_replay = True
            return delegated_error
        return None

    def invalidate_paths(self, paths: list[str]) -> None:
        self.cache.invalidate_paths(paths)
