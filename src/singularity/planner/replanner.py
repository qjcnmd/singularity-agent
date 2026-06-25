from __future__ import annotations

from typing import Any

from singularity.planner.models import ActionKind, ReplanDecision, ReplanDecisionKind


class Replanner:
    def decide(self, signal: dict[str, Any]) -> ReplanDecision:
        contract = signal.get("repair_contract")
        contract = contract if isinstance(contract, dict) else {}
        blocked_reason = contract.get("blocked_reason") or signal.get("blocked_reason")
        if blocked_reason or contract.get("needs_user_input") or signal.get("needs_user_input"):
            return ReplanDecision(
                decision=ReplanDecisionKind.ASK_USER,
                reason=str(blocked_reason or "Repair contract requires user input."),
                next_action=ActionKind.ASK_USER,
            )
        try:
            confidence = float(contract.get("confidence", signal.get("confidence", 1.0)))
        except (TypeError, ValueError):
            return ReplanDecision(
                decision=ReplanDecisionKind.ASK_USER,
                reason="Repair contract confidence is invalid.",
                next_action=ActionKind.ASK_USER,
            )
        if confidence < 0.45:
            return ReplanDecision(
                decision=ReplanDecisionKind.ASK_USER,
                reason="Repair contract confidence is too low to continue automatically.",
                next_action=ActionKind.ASK_USER,
            )
        failure_category = str(signal.get("failure_category") or contract.get("failure_category") or "")
        if failure_category in {
            "approval_required",
            "missing_information",
            "permission_denied",
            "policy_blocked",
            "policy_denied",
            "risk_escalated",
            "sandbox_required",
            "user_input_required",
        }:
            return ReplanDecision(
                decision=ReplanDecisionKind.ASK_USER,
                reason=f"{failure_category} cannot be repaired automatically.",
                next_action=ActionKind.ASK_USER,
            )
        target_files = signal.get("target_files") or contract.get("target_files") or []
        action_candidates = signal.get("action_candidates") or contract.get("action_candidates") or []
        verification_plan = signal.get("verification_plan") or contract.get("verification_plan") or []
        if failure_category and target_files and action_candidates and verification_plan:
            return ReplanDecision(
                decision=ReplanDecisionKind.REPAIR_FAILURE,
                reason=(
                    f"{failure_category} repair contract targets "
                    f"{', '.join(str(item) for item in target_files[:3])}."
                ),
                next_action=ActionKind.REPAIR_CHANGE,
            )
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
