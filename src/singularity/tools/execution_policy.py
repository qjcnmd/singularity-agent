from __future__ import annotations

from typing import Any

from singularity.tools.execution_pipeline import ToolExecutionPipelineState
from singularity.tools.models import ToolResult


class ToolExecutionPolicyGate:
    def __init__(self, executor: Any) -> None:
        self.executor = executor

    def enforce(self, state: ToolExecutionPipelineState) -> ToolResult | None:
        assert state.spec is not None
        assert state.validated_args is not None
        policy_result, approval_grant_id, policy_decision_id = self.executor._enforce_policy(
            tool_name=state.tool_name,
            spec=state.spec,
            validated_args=state.validated_args,
            tool_call_id=state.tool_call_id,
        )
        state.approval_grant_id = approval_grant_id
        state.policy_decision_id = policy_decision_id
        if (
            policy_result is not None
            and self.executor._delegates_policy_decision(state.spec, policy_result)
        ):
            return None
        if policy_result is not None:
            state.remember_replay = True
            return policy_result
        return None
