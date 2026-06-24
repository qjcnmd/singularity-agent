from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass
from typing import Any

from singularity.tool_protocol.models import ToolCallBatch, ToolCallEnvelope
from singularity.tools import ToolResult, ToolRuntime
from singularity.tools.models import ToolExecutionRequest


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
        batch: ToolCallBatch | None = None,
    ) -> list[ParallelToolExecutionResult]:
        if not calls:
            return []
        worker_count = min(len(calls), self.max_workers or len(calls))
        with ThreadPoolExecutor(max_workers=worker_count) as executor:
            futures = {
                executor.submit(_execute_call, tool_runtime, call, batch): (index, call)
                for index, call in enumerate(calls)
            }
            results: list[ParallelToolExecutionResult | None] = [None] * len(calls)
            for future in as_completed(futures):
                index, call = futures[future]
                try:
                    result = future.result()
                except Exception as exc:
                    result = ToolResult.failure(
                        code="parallel_tool_execution_failed",
                        message=str(exc),
                        metadata={"tool_call_id": call.tool_call_id},
                    )
                results[index] = ParallelToolExecutionResult(call=call, result=result)
            return [item for item in results if item is not None]


def _execute_call(
    tool_runtime: ToolRuntime,
    call: ToolCallEnvelope,
    batch: ToolCallBatch | None,
) -> ToolResult:
    if hasattr(tool_runtime, "execute_request"):
        return tool_runtime.execute_request(ToolExecutionRequest.from_envelope(call, batch=batch))
    return tool_runtime.execute_tool_call(call.to_provider_tool_call())
