from __future__ import annotations

from typing import Any

from miniharness.planner.models import ActionKind, ReplanDecision, ReplanDecisionKind


class Replanner:
    def decide(self, signal: dict[str, Any]) -> ReplanDecision:
        error_code = signal.get("error_code") or signal.get("code")
        if error_code in {"patch_context_not_found", "snapshot_mismatch", "external_change_detected"}:
            return ReplanDecision(
                decision=ReplanDecisionKind.READ_FRESH_FILE,
                reason=f"{error_code} requires fresh workspace context.",
                next_action=ActionKind.READ_RELEVANT_FILES,
            )
        if signal.get("verification_failed") or error_code in {"blocked_by_verification", "semantic_failure"}:
            return ReplanDecision(
                decision=ReplanDecisionKind.REPAIR_FAILURE,
                reason="Verification failure requires a repair action.",
                next_action=ActionKind.REPAIR_CHANGE,
            )
        if error_code in {"risk_escalated", "needs_review"}:
            return ReplanDecision(
                decision=ReplanDecisionKind.REQUIRE_REVIEW,
                reason="Risk escalation requires review.",
                next_action=ActionKind.REQUIRE_REVIEW,
            )
        return ReplanDecision(
            decision=ReplanDecisionKind.CONTINUE,
            reason="No replan required.",
        )
