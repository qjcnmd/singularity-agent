from __future__ import annotations

from singularity.review import (
    ReviewCategory,
    ReviewDecisionAction,
    ReviewDecisionEngine,
    ReviewFinding,
    ReviewSeverity,
    ReviewStage,
    ReviewTarget,
)


def finding(
    *,
    severity: str = "error",
    category: str = "bug_risk",
    blocking: bool = True,
) -> ReviewFinding:
    return ReviewFinding(
        title="finding",
        severity=severity,
        category=category,
        evidence=["evidence"],
        recommendation="fix",
        blocking=blocking,
    )


def test_decision_accepts_when_no_blocking_findings_and_verification_ready() -> None:
    target = ReviewTarget(stage=ReviewStage.POST_VERIFICATION, verification_id="vplan_1")

    decision = ReviewDecisionEngine().decide(
        target=target,
        findings=[finding(severity="warning", category="maintainability", blocking=False)],
        context={"verification_status": "ready"},
    )

    assert decision.action == ReviewDecisionAction.ACCEPT


def test_policy_risk_requires_human_approval_before_other_actions() -> None:
    target = ReviewTarget(stage=ReviewStage.PRE_EDIT, policy_decision_id="decision_1")

    decision = ReviewDecisionEngine().decide(
        target=target,
        findings=[finding(category=ReviewCategory.POLICY_RISK.value, blocking=True)],
        context={"policy_outcome": "require_review"},
    )

    assert decision.action == ReviewDecisionAction.NEEDS_HUMAN_APPROVAL
    assert decision.required_approval_decision_id == "decision_1"


def test_failed_required_verification_repairs() -> None:
    target = ReviewTarget(stage=ReviewStage.POST_VERIFICATION, verification_id="vplan_1")

    decision = ReviewDecisionEngine().decide(
        target=target,
        findings=[finding(category=ReviewCategory.BUG_RISK.value, blocking=True)],
        context={"verification_status": "failed", "failed_required_checks": ["check_1"]},
    )

    assert decision.action == ReviewDecisionAction.REPAIR
    assert "check_1" in decision.repair_targets


def test_blocked_verification_replans_instead_of_repairing() -> None:
    target = ReviewTarget(stage=ReviewStage.POST_VERIFICATION, verification_id="vplan_1")

    decision = ReviewDecisionEngine().decide(
        target=target,
        findings=[finding(category=ReviewCategory.VERIFICATION_GAP.value, blocking=True)],
        context={"verification_status": "blocked", "blocked_required_checks": ["check_1"]},
    )

    assert decision.action == ReviewDecisionAction.REPLAN
    assert decision.replan_signal["error_code"] == "review_verification_gap"
    assert decision.replan_signal["gap_checks"] == ["check_1"]


def test_flaky_verification_replans_as_inconclusive_evidence() -> None:
    target = ReviewTarget(stage=ReviewStage.POST_VERIFICATION, verification_id="vplan_1")

    decision = ReviewDecisionEngine().decide(
        target=target,
        findings=[finding(severity="warning", category=ReviewCategory.VERIFICATION_GAP.value, blocking=True)],
        context={"verification_status": "ready_with_warnings", "flaky_required_checks": ["check_1"]},
    )

    assert decision.action == ReviewDecisionAction.REPLAN
    assert decision.replan_signal["gap_checks"] == ["check_1"]


def test_missing_verification_evidence_replans() -> None:
    target = ReviewTarget(stage=ReviewStage.FINAL)

    decision = ReviewDecisionEngine().decide(
        target=target,
        findings=[finding(category=ReviewCategory.VERIFICATION_GAP.value, blocking=True)],
        context={"verification_status": "not_run"},
    )

    assert decision.action == ReviewDecisionAction.REPLAN
    assert decision.replan_signal["error_code"] == "review_verification_gap"


def test_critical_post_patch_with_transaction_rolls_back() -> None:
    target = ReviewTarget(stage=ReviewStage.POST_PATCH, transaction_id="tx_1")

    decision = ReviewDecisionEngine().decide(
        target=target,
        findings=[finding(severity=ReviewSeverity.CRITICAL.value, category="security_risk", blocking=True)],
        context={},
    )

    assert decision.action == ReviewDecisionAction.ROLLBACK
    assert decision.rollback_transaction_id == "tx_1"


def test_critical_finding_blocks_even_when_not_marked_blocking() -> None:
    target = ReviewTarget(stage=ReviewStage.PRE_EDIT)

    decision = ReviewDecisionEngine().decide(
        target=target,
        findings=[
            finding(
                severity=ReviewSeverity.CRITICAL.value,
                category=ReviewCategory.ARCHITECTURE_REGRESSION.value,
                blocking=False,
            )
        ],
        context={},
    )

    assert decision.action == ReviewDecisionAction.REPLAN
    assert decision.replan_signal["error_code"] == "review_critical_findings"
