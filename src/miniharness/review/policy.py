from __future__ import annotations

from typing import Any


def extract_policy_context(policy_observation: dict[str, Any] | None) -> dict[str, Any]:
    if not isinstance(policy_observation, dict):
        return {}
    return {
        "policy_outcome": policy_observation.get("outcome"),
        "policy_decision_id": policy_observation.get("decision_id"),
        "policy_reason": policy_observation.get("reason"),
        "policy_risk_level": policy_observation.get("risk_level"),
    }
