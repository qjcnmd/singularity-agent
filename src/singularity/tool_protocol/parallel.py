from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from typing import Any

from singularity.tool_protocol.models import ToolCallEnvelope
from singularity.tools import ToolResult, ToolRuntime


@dataclass(frozen=True)
class ParallelToolExecutionResult:
    call: ToolCallEnvelope
    result: ToolResult


class ParallelToolExecutor:
    """Bounded executor for parallel read-only tool calls."""

    def __init__(self, *, max_workers: int | None = None) -> None:
        self.max_workers = max_workers

    def execute(
        self,
        calls: list[ToolCallEnvelope],
        *,
        tool_runtime: ToolRuntime,
    ) -> list[ParallelToolExecutionResult]:
        if not calls:
            return []
        worker_count = min(len(calls), self.max_workers or len(calls))
        with ThreadPoolExecutor(max_workers=worker_count) as executor:
            futures = [
                (call, executor.submit(tool_runtime.execute_tool_call, call.to_provider_tool_call()))
                for call in calls
            ]
            results: list[ParallelToolExecutionResult] = []
            for call, future in futures:
                try:
                    result = future.result()
                except Exception as exc:
                    result = ToolResult.failure(
                        code="parallel_tool_execution_failed",
                        message=str(exc),
                        metadata={"tool_call_id": call.tool_call_id},
                    )
                results.append(ParallelToolExecutionResult(call=call, result=result))
            return results
