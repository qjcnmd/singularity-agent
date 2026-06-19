from __future__ import annotations

from typing import Any

from miniharness.review.models import (
    ReviewCategory,
    ReviewDecision,
    ReviewDecisionAction,
    ReviewFinding,
    ReviewSeverity,
    ReviewStage,
    ReviewTarget,
)


class ReviewDecisionEngine:
    def decide(
        self,
        *,
        target: ReviewTarget,
        findings: list[ReviewFinding],
        context: dict[str, Any] | None = None,
    ) -> ReviewDecision:
        context = context or {}
        blocking = [finding for finding in findings if finding.blocking]
        critical = [finding for finding in findings if finding.severity == ReviewSeverity.CRITICAL]
        errors = [finding for finding in findings if finding.severity == ReviewSeverity.ERROR]
        policy_risks = [finding for finding in findings if finding.category == ReviewCategory.POLICY_RISK]
        failed_required_checks = list(context.get("failed_required_checks") or [])
        blocked_required_checks = list(context.get("blocked_required_checks") or [])
        flaky_required_checks = list(context.get("flaky_required_checks") or [])
        verification_status = str(context.get("verification_status") or "").lower()
        policy_outcome = str(context.get("policy_outcome") or "").lower()

        if policy_risks or policy_outcome in {"require_review", "ask_user", "escalate", "sandbox_required", "deny"}:
            return ReviewDecision(
                action=ReviewDecisionAction.NEEDS_HUMAN_APPROVAL,
                reasons=_reasons(policy_risks or blocking or findings, fallback="Policy risk requires human approval."),
                finding_ids=_ids(policy_risks or blocking),
                requires_human_approval=True,
                required_approval_decision_id=target.policy_decision_id or context.get("policy_decision_id"),
                confidence=0.95,
                next_actions=["request_human_approval"],
            )

        if critical and target.transaction_id and target.stage in {ReviewStage.POST_PATCH, ReviewStage.FINAL}:
            return ReviewDecision(
                action=ReviewDecisionAction.ROLLBACK,
                reasons=_reasons(critical, fallback="Critical post-apply risk requires rollback signal."),
                finding_ids=_ids(critical),
                rollback_transaction_id=target.transaction_id,
                confidence=0.9,
                next_actions=["signal_rollback"],
            )

        if verification_status == "failed" or failed_required_checks:
            repair_targets = failed_required_checks or [
                str(finding.metadata.get("check_id"))
                for finding in blocking
                if finding.metadata.get("check_id")
            ]
            return ReviewDecision(
                action=ReviewDecisionAction.REPAIR,
                reasons=_reasons(blocking or errors, fallback="Verification failed required checks."),
                finding_ids=_ids(blocking or errors),
                repair_targets=sorted(set(repair_targets)),
                replan_signal={"verification_failed": True, "failed_checks": sorted(set(repair_targets))},
                confidence=0.9,
                next_actions=["repair_failures", "rerun_verification"],
            )

        if verification_status in {"blocked", "needs_review"} or blocked_required_checks or flaky_required_checks:
            gap_targets = blocked_required_checks or flaky_required_checks
            return ReviewDecision(
                action=ReviewDecisionAction.REPLAN,
                reasons=_reasons(blocking or errors, fallback="Verification evidence is blocked or inconclusive."),
                finding_ids=_ids(blocking or errors),
                replan_signal={
                    "error_code": "review_verification_gap",
                    "verification_status": verification_status or "unknown",
                    "gap_checks": sorted(set(gap_targets)),
                },
                confidence=0.85,
                next_actions=["replan"],
            )

        if critical:
            return ReviewDecision(
                action=ReviewDecisionAction.REPLAN,
                reasons=_reasons(critical, fallback="Critical review findings require replanning."),
                finding_ids=_ids(critical),
                replan_signal={"error_code": "review_critical_findings"},
                confidence=0.9,
                next_actions=["replan"],
            )

        if blocking and target.transaction_id and _rollback_eligible(blocking) and target.stage in {ReviewStage.POST_PATCH, ReviewStage.FINAL}:
            return ReviewDecision(
                action=ReviewDecisionAction.ROLLBACK,
                reasons=_reasons(blocking, fallback="Blocking post-apply risk cannot continue safely."),
                finding_ids=_ids(blocking),
                rollback_transaction_id=target.transaction_id,
                confidence=0.85,
                next_actions=["signal_rollback"],
            )

        if blocking:
            signal = _replan_signal(blocking, verification_status=verification_status)
            return ReviewDecision(
                action=ReviewDecisionAction.REPLAN,
                reasons=_reasons(blocking, fallback="Blocking review findings require replanning."),
                finding_ids=_ids(blocking),
                replan_signal=signal,
                confidence=0.85,
                next_actions=["replan"],
            )

        if errors:
            return ReviewDecision(
                action=ReviewDecisionAction.REPAIR,
                reasons=_reasons(errors, fallback="Non-blocking errors should be repaired."),
                finding_ids=_ids(errors),
                confidence=0.75,
                next_actions=["repair"],
            )

        return ReviewDecision(
            action=ReviewDecisionAction.ACCEPT,
            reasons=["No blocking review findings."],
            finding_ids=[],
            confidence=0.9 if verification_status in {"", "ready", "ready_with_warnings"} else 0.75,
            next_actions=["continue"],
        )


def _ids(findings: list[ReviewFinding]) -> list[str]:
    return [finding.finding_id for finding in findings]


def _reasons(findings: list[ReviewFinding], *, fallback: str) -> list[str]:
    values = [finding.title for finding in findings[:5]]
    return values or [fallback]


def _rollback_eligible(findings: list[ReviewFinding]) -> bool:
    return any(
        finding.category
        in {
            ReviewCategory.SECURITY_RISK,
            ReviewCategory.ARCHITECTURE_REGRESSION,
            ReviewCategory.BUG_RISK,
        }
        and finding.severity in {ReviewSeverity.ERROR, ReviewSeverity.CRITICAL}
        for finding in findings
    )


def _replan_signal(findings: list[ReviewFinding], *, verification_status: str) -> dict[str, Any]:
    categories = {finding.category for finding in findings}
    if ReviewCategory.VERIFICATION_GAP in categories:
        return {"error_code": "review_verification_gap", "verification_status": verification_status or "not_run"}
    if ReviewCategory.GOAL_MISMATCH in categories:
        return {"error_code": "review_goal_mismatch"}
    if ReviewCategory.OVER_EDITING in categories:
        return {"error_code": "review_over_editing"}
    return {"error_code": "review_blocking_findings"}
