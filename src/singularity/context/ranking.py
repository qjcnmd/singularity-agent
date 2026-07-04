from __future__ import annotations

from singularity.context.models import ContextAuthority, ContextLayer

CONTEXT_LAYER_WEIGHTS: dict[ContextLayer, float] = {
    ContextLayer.SYSTEM: 100.0,
    ContextLayer.USER_GOAL: 90.0,
    ContextLayer.TASK_STATE: 40.0,
    ContextLayer.PLANNER_STATE: 38.0,
    ContextLayer.POLICY_STATE: 36.0,
    ContextLayer.VERIFICATION: 34.0,
    ContextLayer.FAILURE_MEMORY: 32.0,
    ContextLayer.WORKSPACE_STATE: 30.0,
    ContextLayer.EVIDENCE: 26.0,
    ContextLayer.TOOL_OBSERVATIONS: 24.0,
    ContextLayer.COMPRESSED_HISTORY: 22.0,
    ContextLayer.RECENT_DIALOGUE: 10.0,
    ContextLayer.REFERENCES: 8.0,
    ContextLayer.SCRATCHPAD: 0.0,
}

CONTEXT_AUTHORITY_WEIGHTS: dict[ContextAuthority, float] = {
    ContextAuthority.SYSTEM: 10.0,
    ContextAuthority.USER: 9.0,
    ContextAuthority.COMPONENT: 7.0,
    ContextAuthority.TOOL: 6.0,
    ContextAuthority.SUMMARY: 4.0,
    ContextAuthority.MODEL: 1.0,
}

CONTEXT_FRONT_LAYER_ORDER: dict[ContextLayer, int] = {
    ContextLayer.SYSTEM: 0,
    ContextLayer.USER_GOAL: 1,
    ContextLayer.COMPRESSED_HISTORY: 2,
    ContextLayer.TASK_STATE: 3,
    ContextLayer.PLANNER_STATE: 4,
}


def layer_weight(layer: ContextLayer) -> float:
    return CONTEXT_LAYER_WEIGHTS.get(layer, 0.0)


def authority_weight(authority: ContextAuthority) -> float:
    return CONTEXT_AUTHORITY_WEIGHTS.get(authority, 0.0)


def layer_order(layer: ContextLayer) -> int:
    return CONTEXT_FRONT_LAYER_ORDER.get(layer, 50)
