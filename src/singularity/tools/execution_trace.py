from __future__ import annotations

import time
from datetime import UTC, datetime
from typing import Any

from singularity.observability.models import TraceEventType, TraceSeverity
from singularity.observability.redaction import TraceRedactor
from singularity.tools.execution_pipeline import ToolExecutionPipelineState
from singularity.tools.models import ToolExecutionRequest, ToolResult, ToolSpec


class ToolExecutionTraceRecorder:
    def __init__(
        self,
        *,
        trace: Any | None,
        planner: Any | None,
        redactor: TraceRedactor,
        annotate_result_metadata: Any,
        argument_summary: Any,
        request_trace_ids: Any,
        emit_trace: Any,
        safe_update_planner: Any,
    ) -> None:
        self.trace = trace
        self.planner = planner
        self.redactor = redactor
        self.annotate_result_metadata = annotate_result_metadata
        self.argument_summary = argument_summary
        self.request_trace_ids = request_trace_ids
        self.emit_trace = emit_trace
        self.safe_update_planner = safe_update_planner

    def finalize(self, state: ToolExecutionPipelineState) -> None:
        result = state.result
        if result is None:
            return
        duration_seconds = time.perf_counter() - state.started
        result.metadata.setdefault("cache_hit", state.cache_hit)
        result.metadata.setdefault("duration_seconds", duration_seconds)
        result.metadata.setdefault("output_digest", state.output_digest)
        self.annotate_result_metadata(result, state.request)
        if state.spec is not None:
            result.metadata.setdefault("backend", state.spec.execution_backend.value)
        if not state.planner_updated and not state.defer_planner_update:
            self.safe_update_planner(
                tool_call_id=state.tool_call_id,
                tool_name=state.tool_name,
                result=result,
                action_id=state.planner_action_id,
            )
        self.record_trace(
            request=state.request,
            tool_call_id=state.tool_call_id,
            tool_name=state.tool_name,
            spec=state.spec,
            validated_args=state.validated_args,
            started_at=state.started_at,
            ended_at=datetime.now(UTC).isoformat(),
            duration_seconds=duration_seconds,
            result=result,
            output_digest=state.output_digest,
            cache_hit=state.cache_hit,
        )

    def record_trace(
        self,
        *,
        request: ToolExecutionRequest,
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
        args_summary = (
            self.argument_summary(validated_args)
            if validated_args is not None
            else None
        )
        payload = {
            "tool_call_id": tool_call_id,
            "tool_name": tool_name,
            "batch_id": request.batch_id,
            "run_id": request.run_id,
            "session_id": request.session_id,
            "task_id": request.task_id,
            "phase_id": request.phase_id,
            "model_request_id": request.model_request_id,
            "model_response_id": request.model_response_id,
            "argument_digest": request.argument_digest,
            "policy_decision_id": result.metadata.get("policy_decision_id"),
            "argument_summary": args_summary,
            "permission_level": spec.permission_level.value if spec is not None else None,
            "risk_tags": list(spec.risk_tags) if spec is not None else [],
            "start": started_at,
            "end": ended_at,
            "duration_seconds": duration_seconds,
            "status": "ok" if result.ok else "error",
            "error_code": result.error_code,
            "truncated": result.truncated,
            "output_digest": output_digest,
            "cache_hit": cache_hit,
            "backend": spec.execution_backend.value if spec is not None else None,
        }
        if not hasattr(self.trace, "emit"):
            self.trace.record("tool_call", payload)
            return
        self.emit_trace(
            TraceEventType.TOOL_DISPATCH_COMPLETED if result.ok else TraceEventType.TOOL_DISPATCH_FAILED,
            summary=f"Tool {tool_name} {'completed' if result.ok else 'failed'}.",
            payload=payload,
            ids=self.request_trace_ids(request, action_id=tool_call_id),
            severity=TraceSeverity.INFO if result.ok else TraceSeverity.ERROR,
        )
