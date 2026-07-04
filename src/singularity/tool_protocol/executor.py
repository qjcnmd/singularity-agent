from __future__ import annotations

from typing import Any

from singularity.context import ContextManager
from singularity.planner import Planner
from singularity.tool_protocol.models import ToolExecutionPlan, ToolProtocolTurnResult
from singularity.tools import ToolExecutor


class ToolProtocolPlanExecutor:
    def __init__(self, engine: Any) -> None:
        self.engine = engine

    def execute(
        self,
        plan: ToolExecutionPlan,
        *,
        context: ContextManager,
        tool_executor: ToolExecutor,
        planner: Planner | None,
        turn: int = 0,
    ) -> ToolProtocolTurnResult:
        return self.engine._execute_plan_impl(
            plan,
            context=context,
            tool_executor=tool_executor,
            planner=planner,
            turn=turn,
        )
