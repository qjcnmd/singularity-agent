from __future__ import annotations

import hashlib
import json
from pathlib import Path
from typing import Any

from singularity.observability.redaction import TraceRedactor
from singularity.tools.cache import ToolResultCache
from singularity.tools.execution_pipeline import ToolExecutionPipelineState
from singularity.tools.execution_resources import resources_for, touched_paths
from singularity.tools.models import (
    PermissionLevel,
    ToolExecutionBackendKind,
    ToolResult,
    ToolSensitivityLevel,
    ToolSpec,
)


class ToolExecutionCache:
    def __init__(
        self,
        *,
        cache: ToolResultCache,
        workspace_root: Path,
        redactor: TraceRedactor,
        result_digest: Any,
        output_text: Any,
        throw_if_cancelled: Any,
        standalone_can_execute: bool,
    ) -> None:
        self.cache = cache
        self.workspace_root = workspace_root
        self.redactor = redactor
        self.result_digest = result_digest
        self.output_text = output_text
        self.throw_if_cancelled = throw_if_cancelled
        self.standalone_can_execute = standalone_can_execute

    def precheck(self, state: ToolExecutionPipelineState) -> ToolResult | None:
        assert state.spec is not None
        assert state.validated_args is not None
        self.throw_if_cancelled()
        state.cache_key = self.cache_key(state.spec, state.validated_args)
        cache_policy = state.spec.cache_policy
        if self.should_cache(state.spec):
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
                state.output_digest = cached.metadata.get("output_digest") or self.result_digest(cached)
                state.remember_replay = True
                return cached

        delegated_error = self.delegated_backend_error(state.spec)
        if delegated_error is not None:
            state.remember_replay = True
            return delegated_error
        return None

    def set(
        self,
        cache_key: str,
        result: ToolResult,
        *,
        max_entries: int,
        touched_paths: tuple[str, ...],
    ) -> None:
        self.cache.set(
            cache_key,
            result,
            max_entries=max_entries,
            touched_paths=touched_paths,
        )

    def invalidate_paths(self, paths: list[str]) -> None:
        self.cache.invalidate_paths(paths)

    def clear(self) -> None:
        self.cache.clear()

    def cache_key(self, spec: ToolSpec, validated_args: dict[str, Any]) -> str:
        payload = {
            "tool_name": spec.name,
            "version": spec.version,
            "schema": _model_schema_fingerprint(spec),
            "arguments": validated_args,
            "workspace_root": str(self.workspace_root),
            "paths": self.file_snapshots(spec, validated_args),
        }
        return json.dumps(payload, ensure_ascii=False, sort_keys=True, default=str)

    def file_snapshots(self, spec: ToolSpec, args: dict[str, Any]) -> dict[str, Any]:
        snapshots: dict[str, Any] = {}
        for path in touched_paths(spec, args, self.workspace_root):
            full = (self.workspace_root / path).resolve(strict=False)
            try:
                stat = full.stat()
            except OSError:
                snapshots[path] = {"exists": False}
                continue
            snapshots[path] = {
                "size": stat.st_size,
                "mtime_ns": stat.st_mtime_ns,
                "is_dir": full.is_dir(),
            }
        return snapshots

    def touched_paths(self, spec: ToolSpec, args: dict[str, Any]) -> tuple[str, ...]:
        return touched_paths(spec, args, self.workspace_root)

    def should_cache(self, spec: ToolSpec) -> bool:
        return (
            bool(spec.cache_policy and spec.cache_policy.cacheable)
            and spec.permission_level == PermissionLevel.READ_ONLY
            and bool(spec.idempotency_policy and spec.idempotency_policy.idempotent)
            and spec.sensitivity not in {
                ToolSensitivityLevel.SENSITIVE,
                ToolSensitivityLevel.SECRET,
            }
        )

    def is_sensitive_result(self, spec: ToolSpec, result: ToolResult) -> bool:
        if spec.sensitivity in {ToolSensitivityLevel.SENSITIVE, ToolSensitivityLevel.SECRET}:
            return True
        text = self.output_text(result.content)
        return self.redactor.redact_text(text) != text

    def delegated_backend_error(self, spec: ToolSpec) -> ToolResult | None:
        if spec.execution_backend == ToolExecutionBackendKind.IN_PROCESS:
            return None
        if self.standalone_can_execute:
            return None
        return ToolResult.failure(
            code="delegated_backend_unavailable",
            message=f"Delegated backend is unavailable: {spec.execution_backend.value}",
            metadata={"backend": spec.execution_backend.value},
        )

    def resources_for(self, spec: ToolSpec, args: dict[str, Any]) -> list[Any]:
        return resources_for(spec, args, self.workspace_root)


def _model_schema_fingerprint(spec: ToolSpec) -> str:
    schema = spec.input_model.model_json_schema()
    text = json.dumps(schema, ensure_ascii=False, sort_keys=True, default=str)
    return hashlib.sha256(text.encode("utf-8")).hexdigest()
