from __future__ import annotations

from typing import Any

from singularity.observability.models import TraceEventType
from singularity.tools.execution_pipeline import ToolExecutionPipelineState
from singularity.tools.models import PermissionLevel, ToolResult


class ToolExecutionDispatcher:
    def __init__(self, executor: Any) -> None:
        self.executor = executor

    def dispatch(self, state: ToolExecutionPipelineState) -> ToolResult:
        assert state.spec is not None
        assert state.validated is not None
        assert state.validated_args is not None
        self.executor._emit_trace(
            TraceEventType.TOOL_DISPATCH_STARTED,
            summary=f"Dispatching tool {state.tool_name}.",
            payload={
                "tool_name": state.tool_name,
                "tool_call_id": state.tool_call_id,
                "permission_level": state.spec.permission_level.value,
                "risk_tags": list(state.spec.risk_tags),
                "backend": state.spec.execution_backend.value,
                "batch_id": state.request.batch_id,
                "argument_digest": state.request.argument_digest,
                "arguments": self.executor._argument_trace_summary(state.validated_args),
            },
            ids=self.executor._request_trace_ids(
                state.request,
                action_id=state.planner_action_id or state.tool_call_id,
            ),
        )
        self.executor._throw_if_cancelled()
        result, state.output_digest = self.executor._execute_handler(state.spec, state.validated)
        self.executor._throw_if_cancelled()
        if state.approval_grant_id:
            result.metadata["approval_grant_id"] = state.approval_grant_id
        if state.policy_decision_id:
            result.metadata["policy_decision_id"] = state.policy_decision_id
        self.executor._update_planner(
            tool_call_id=state.tool_call_id,
            tool_name=state.tool_name,
            result=result,
            action_id=state.planner_action_id,
        )
        state.planner_updated = True
        if (
            self.executor._should_cache(state.spec)
            and result.ok
            and not self.executor._is_sensitive_result(state.spec, result)
        ):
            assert state.cache_key is not None
            touched_paths = self.executor._touched_paths(state.spec, state.validated_args)
            self.executor._cache.set(
                state.cache_key,
                result,
                max_entries=state.spec.cache_policy.max_entries
                if state.spec.cache_policy
                else 128,
                touched_paths=touched_paths,
            )
        if state.spec.permission_level != PermissionLevel.READ_ONLY:
            self.executor._invalidate_after_write(state.spec, state.validated_args, result)
        state.remember_replay = True
        return result
