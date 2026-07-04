from __future__ import annotations

from dataclasses import dataclass
from typing import Any

from singularity.tools.models import ToolExecutionRequest, ToolResult, ToolSpec


@dataclass
class ToolExecutionPipelineState:
    request: ToolExecutionRequest
    started_at: str
    started: float
    tool_call_id: str | None
    tool_name: str
    spec: ToolSpec | None = None
    parsed_args: Any | None = None
    validated: Any | None = None
    validated_args: dict[str, Any] | None = None
    args_fingerprint: str | None = None
    planner_action_id: str | None = None
    approval_grant_id: str | None = None
    policy_decision_id: str | None = None
    cache_key: str | None = None
    cache_hit: bool = False
    output_digest: str | None = None
    result: ToolResult | None = None
    planner_updated: bool = False
    remember_replay: bool = False
