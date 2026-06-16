from __future__ import annotations

import hashlib
import json
import time
from concurrent.futures import ThreadPoolExecutor, TimeoutError as FutureTimeout
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from pydantic import ValidationError

from miniharness.tools.models import (
    PermissionLevel,
    ToolError,
    ToolExecutionFailure,
    ToolResult,
    ToolSpec,
)
from miniharness.tools.policy import ToolPolicy
from miniharness.tools.registry import ToolRegistry
from miniharness.trace import TraceWriter


class ToolRuntime:
    def __init__(
        self,
        *,
        registry: ToolRegistry,
        policy: ToolPolicy,
        trace: TraceWriter | None,
        workspace_root: Path,
    ) -> None:
        self.registry = registry
        self.policy = policy
        self.trace = trace
        self.workspace_root = workspace_root.resolve()
        self._cache: dict[str, ToolResult] = {}

    def execute_tool_call(self, tool_call: dict[str, Any]) -> ToolResult:
        started_at = datetime.now(UTC).isoformat()
        started = time.perf_counter()
        tool_call_id = tool_call.get("id")
        function = tool_call.get("function") or {}
        tool_name = function.get("name") or "<unknown>"
        spec: ToolSpec | None = None
        validated_args: dict[str, Any] | None = None
        cache_hit = False
        output_digest: str | None = None

        try:
            spec = self.registry.get(tool_name)
            if spec is None:
                result = ToolResult.failure(
                    code="tool_not_found",
                    message=f"Unknown tool: {tool_name}",
                )
                output_digest = self._result_digest(result)
                return result

            raw_arguments = function.get("arguments") or "{}"
            try:
                arguments = self._parse_arguments(raw_arguments)
            except json.JSONDecodeError as exc:
                result = ToolResult.failure(
                    code="bad_arguments_json",
                    message=f"Invalid JSON arguments: {exc}",
                )
                output_digest = self._result_digest(result)
                return result

            try:
                validated = spec.input_model.model_validate(arguments)
            except ValidationError as exc:
                result = ToolResult.failure(
                    code="validation_error",
                    message="Tool arguments failed validation.",
                    details=exc.errors(),
                )
                output_digest = self._result_digest(result)
                return result
            validated_args = validated.model_dump(mode="json")

            policy_error = self.policy.check(spec)
            if policy_error is not None:
                result = ToolResult.failure(
                    code=policy_error.code,
                    message=policy_error.message,
                    details=policy_error.details,
                )
                output_digest = self._result_digest(result)
                return result
            if (
                spec.permission_level == PermissionLevel.WRITE
                and not spec.uses_mutation_runtime
            ):
                result = ToolResult.failure(
                    code="invalid_operation",
                    message=(
                        "Write tools must execute through Workspace Mutation Runtime."
                    ),
                    details={"tool_name": spec.name},
                )
                output_digest = self._result_digest(result)
                return result
            if (
                spec.permission_level == PermissionLevel.SHELL
                and not spec.uses_command_runtime
            ):
                result = ToolResult.failure(
                    code="invalid_operation",
                    message="Shell tools must execute through CommandRuntime.",
                    details={"tool_name": spec.name},
                )
                output_digest = self._result_digest(result)
                return result

            cache_key = self._cache_key(spec, validated_args)
            if self._should_cache(spec) and cache_key in self._cache:
                cache_hit = True
                result = self._cache[cache_key].model_copy(deep=True)
                result.metadata["cache_hit"] = True
                output_digest = result.metadata.get("output_digest")
                return result

            result, output_digest = self._execute_handler(spec, validated)
            if self._should_cache(spec) and result.ok:
                self._cache[cache_key] = result.model_copy(deep=True)
            return result
        except Exception as exc:
            result = ToolResult.failure(
                code="internal_error",
                message=str(exc),
                details={"type": type(exc).__name__},
            )
            output_digest = self._result_digest(result)
            return result
        finally:
            ended_at = datetime.now(UTC).isoformat()
            duration_seconds = time.perf_counter() - started
            if "result" in locals():
                result.metadata.setdefault("cache_hit", cache_hit)
                result.metadata.setdefault("duration_seconds", duration_seconds)
                result.metadata.setdefault("output_digest", output_digest)
                self._record_trace(
                    tool_call_id=tool_call_id,
                    tool_name=tool_name,
                    spec=spec,
                    validated_args=validated_args,
                    started_at=started_at,
                    ended_at=ended_at,
                    duration_seconds=duration_seconds,
                    result=result,
                    output_digest=output_digest,
                    cache_hit=cache_hit,
                )

    def _execute_handler(
        self, spec: ToolSpec, validated_args: Any
    ) -> tuple[ToolResult, str]:
        executor = ThreadPoolExecutor(max_workers=1)
        future = executor.submit(spec.handler, validated_args)
        try:
            output = future.result(timeout=spec.timeout_seconds)
        except FutureTimeout:
            future.cancel()
            result = ToolResult.failure(
                code="timeout",
                message=f"Tool timed out after {spec.timeout_seconds} seconds.",
            )
            return result, self._result_digest(result)
        except ToolExecutionFailure as exc:
            result = ToolResult.failure(
                code=exc.code,
                message=exc.message,
                details=exc.details,
            )
            return result, self._result_digest(result)
        except Exception as exc:
            result = ToolResult.failure(
                code="execution_error",
                message=str(exc),
                details={"type": type(exc).__name__},
            )
            return result, self._result_digest(result)
        finally:
            executor.shutdown(wait=False, cancel_futures=True)

        content, truncated, metadata, digest = self._limit_output(
            output, spec.max_output_chars
        )
        result = ToolResult.success(
            content=content,
            truncated=truncated,
            metadata=metadata,
        )
        result.metadata["output_digest"] = digest
        return result, digest

    def _record_trace(
        self,
        *,
        tool_call_id: str | None,
        tool_name: str,
        spec: ToolSpec | None,
        validated_args: dict[str, Any] | None,
        started_at: str,
        ended_at: str,
        duration_seconds: float,
        result: ToolResult,
        output_digest: str | None,
        cache_hit: bool,
    ) -> None:
        if self.trace is None:
            return

        self.trace.record(
            "tool_call",
            {
                "tool_call_id": tool_call_id,
                "tool_name": tool_name,
                "validated_args": validated_args,
                "permission_level": (
                    spec.permission_level.value if spec is not None else None
                ),
                "risk_tags": list(spec.risk_tags) if spec is not None else [],
                "start": started_at,
                "end": ended_at,
                "duration_seconds": duration_seconds,
                "status": "ok" if result.ok else "error",
                "error_code": result.error_code,
                "truncated": result.truncated,
                "output_digest": output_digest,
                "cache_hit": cache_hit,
            },
        )

    @staticmethod
    def _parse_arguments(raw_arguments: Any) -> Any:
        if isinstance(raw_arguments, dict):
            return raw_arguments
        return json.loads(raw_arguments)

    @staticmethod
    def _should_cache(spec: ToolSpec) -> bool:
        return spec.cacheable and spec.permission_level == PermissionLevel.READ_ONLY

    def _cache_key(self, spec: ToolSpec, validated_args: dict[str, Any]) -> str:
        payload = {
            "tool_name": spec.name,
            "version": spec.version,
            "arguments": validated_args,
            "workspace_root": str(self.workspace_root),
        }
        return json.dumps(payload, ensure_ascii=False, sort_keys=True, default=str)

    def _limit_output(
        self, output: Any, max_output_chars: int
    ) -> tuple[Any, bool, dict[str, Any], str]:
        text = self._output_text(output)
        digest = self._digest(text)
        original_chars = len(text)
        if original_chars <= max_output_chars:
            return output, False, {
                "original_chars": original_chars,
                "returned_chars": original_chars,
                "cache_hit": False,
            }, digest

        truncated = self._truncate_head_tail(text, max_output_chars)
        return truncated, True, {
            "original_chars": original_chars,
            "returned_chars": len(truncated),
            "cache_hit": False,
        }, digest

    @staticmethod
    def _truncate_head_tail(text: str, max_chars: int) -> str:
        marker = "\n...[truncated]...\n"
        if max_chars <= len(marker) + 2:
            return text[:max_chars]
        head_chars = (max_chars - len(marker)) // 2
        tail_chars = max_chars - len(marker) - head_chars
        return f"{text[:head_chars]}{marker}{text[-tail_chars:]}"

    @staticmethod
    def _output_text(output: Any) -> str:
        if isinstance(output, str):
            return output
        return json.dumps(output, ensure_ascii=False, sort_keys=True, default=str)

    @staticmethod
    def _digest(text: str) -> str:
        return hashlib.sha256(text.encode("utf-8")).hexdigest()

    def _result_digest(self, result: ToolResult) -> str:
        dumped = result.model_dump(mode="json")
        return self._digest(json.dumps(dumped, ensure_ascii=False, sort_keys=True))
