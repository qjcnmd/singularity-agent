from __future__ import annotations

from typing import Any

from miniharness.review.evidence import to_bounded_plain
from miniharness.review.models import (
    ReviewCategory,
    ReviewEvidence,
    ReviewFinding,
    ReviewSeverity,
    ReviewStage,
    ReviewTarget,
)


VALIDATION_CATEGORY_MAP = {
    "diff_budget": ReviewCategory.OVER_EDITING,
    "over_modification": ReviewCategory.OVER_EDITING,
    "policy_denied": ReviewCategory.POLICY_RISK,
    "review_required": ReviewCategory.POLICY_RISK,
    "syntax_risk": ReviewCategory.BUG_RISK,
    "format_risk": ReviewCategory.STYLE,
    "context_mismatch": ReviewCategory.GOAL_MISMATCH,
    "freshness": ReviewCategory.VERIFICATION_GAP,
}


class RuleFindingCollector:
    def collect(
        self,
        *,
        target: ReviewTarget,
        evidence: list[ReviewEvidence],
        context: dict[str, Any],
    ) -> list[ReviewFinding]:
        findings: list[ReviewFinding] = []
        findings.extend(self._from_validation(target, evidence, context))
        findings.extend(self._from_impact(target, evidence, context))
        findings.extend(self._from_verification(target, evidence, context))
        findings.extend(self._from_policy(evidence, context))
        findings.extend(self._from_final_gap(target, context))
        return _dedupe_findings(findings)

    def _from_validation(
        self,
        target: ReviewTarget,
        evidence: list[ReviewEvidence],
        context: dict[str, Any],
    ) -> list[ReviewFinding]:
        validation = _dict(context.get("validation"))
        if not validation:
            return []
        findings: list[ReviewFinding] = []
        validation_evidence = _evidence_ids(evidence, {"validation", "patch_validation"})
        issues = validation.get("issues") or []
        failure_category = str(validation.get("failure_category") or "")
        category = VALIDATION_CATEGORY_MAP.get(failure_category, ReviewCategory.BUG_RISK)
        if validation.get("requires_review"):
            findings.append(
                ReviewFinding(
                    title="Patch validation requires review",
                    severity=ReviewSeverity.ERROR if target.stage == ReviewStage.PRE_EDIT else ReviewSeverity.WARNING,
                    category=category,
                    evidence=[
                        f"Patch validation requires review; category={failure_category or 'unknown'}.",
                        *[_issue_summary(issue) for issue in issues[:5]],
                    ],
                    evidence_ids=validation_evidence,
                    recommendation="Narrow the edit, refresh context, or request approval before applying.",
                    blocking=True,
                )
            )
        if validation.get("ok") is False and not validation.get("requires_review"):
            findings.append(
                ReviewFinding(
                    title="Patch validation failed",
                    severity=ReviewSeverity.ERROR,
                    category=category,
                    evidence=[_issue_summary(issue) for issue in issues[:5]] or ["Patch validation returned ok=false."],
                    evidence_ids=validation_evidence,
                    recommendation="Repair the patch before previewing or applying it.",
                    blocking=True,
                )
            )
        return findings

    def _from_impact(
        self,
        target: ReviewTarget,
        evidence: list[ReviewEvidence],
        context: dict[str, Any],
    ) -> list[ReviewFinding]:
        findings: list[ReviewFinding] = []
        code_impact = _dict(context.get("code_impact"))
        test_impact = _dict(context.get("test_impact"))
        changed_files = list(context.get("changed_files") or [])
        impact_ids = _evidence_ids(evidence, {"code_impact", "project_index"})
        if code_impact:
            risk_level = str(code_impact.get("risk_level") or "").lower()
            if code_impact.get("broad_impact") or risk_level in {"high", "critical"}:
                findings.append(
                    ReviewFinding(
                        title="Patch has broad code impact",
                        severity=ReviewSeverity.ERROR if risk_level == "critical" else ReviewSeverity.WARNING,
                        category=ReviewCategory.ARCHITECTURE_REGRESSION,
                        evidence=[
                            f"Code impact risk_level={risk_level or 'unknown'} broad_impact={bool(code_impact.get('broad_impact'))}.",
                            *[str(reason) for reason in (code_impact.get("risk_reasons") or [])[:5]],
                        ],
                        evidence_ids=impact_ids,
                        recommendation="Confirm affected dependents and include validation covering the impacted surface.",
                        blocking=target.stage in {ReviewStage.POST_PATCH, ReviewStage.FINAL} and risk_level == "critical",
                    )
                )
            if code_impact.get("generated_or_vendor_impact"):
                findings.append(
                    ReviewFinding(
                        title="Patch touches generated or vendor-managed files",
                        severity=ReviewSeverity.WARNING,
                        category=ReviewCategory.MAINTAINABILITY,
                        evidence=["Project index marked generated_or_vendor_impact=true."],
                        evidence_ids=impact_ids,
                        recommendation="Avoid editing generated/vendor files unless explicitly required.",
                        blocking=False,
                    )
                )
        if target.stage in {ReviewStage.POST_PATCH, ReviewStage.FINAL} and changed_files and not test_impact:
            findings.append(
                ReviewFinding(
                    title="No test impact evidence is available",
                    severity=ReviewSeverity.WARNING,
                    category=ReviewCategory.TEST_GAP,
                    evidence=["Changed files exist but no test impact evidence was provided."],
                    recommendation="Plan or run verification covering the changed files.",
                    blocking=False,
                )
            )
        elif test_impact:
            likely_tests = test_impact.get("likely_tests") or test_impact.get("commands") or []
            confidence = str(test_impact.get("confidence") or test_impact.get("confidence_note") or "")
            if changed_files and not likely_tests and target.stage in {ReviewStage.POST_PATCH, ReviewStage.FINAL}:
                findings.append(
                    ReviewFinding(
                        title="No targeted tests were mapped for changed files",
                        severity=ReviewSeverity.WARNING,
                        category=ReviewCategory.TEST_GAP,
                        evidence=[f"Changed files={changed_files[:10]}; mapping confidence={confidence or 'unknown'}."],
                        evidence_ids=_evidence_ids(evidence, {"test_impact"}),
                        recommendation="Run broader verification or add focused tests for the changed surface.",
                        blocking=False,
                    )
                )
        return findings

    def _from_verification(
        self,
        target: ReviewTarget,
        evidence: list[ReviewEvidence],
        context: dict[str, Any],
    ) -> list[ReviewFinding]:
        verification = _dict(context.get("verification"))
        if not verification:
            return []
        findings: list[ReviewFinding] = []
        assessment = _dict(verification.get("completion_assessment"))
        failed_checks = list(verification.get("failed_checks") or [])
        check_status = list(verification.get("check_status") or [])
        status = str(assessment.get("status") or verification.get("status") or "").lower()
        evidence_ids = _evidence_ids(evidence, {"verification", "verification_result"})
        if status in {"failed", "blocked"} or failed_checks:
            for failed in failed_checks or [item for item in check_status if item.get("status") in {"failed", "blocked", "timeout", "flaky"}]:
                category = ReviewCategory.BUG_RISK
                if failed.get("status") in {"blocked", "timeout"} or failed.get("failure_type") in {"missing_command", "check_blocked", "inconclusive_result"}:
                    category = ReviewCategory.VERIFICATION_GAP
                findings.append(
                    ReviewFinding(
                        title=f"Verification check {failed.get('status') or 'failed'}",
                        severity=ReviewSeverity.ERROR if failed.get("status") != "flaky" else ReviewSeverity.WARNING,
                        category=category,
                        location={"detail": str(failed.get("kind") or failed.get("check_id") or "verification")},
                        evidence=[_verification_summary(failed)],
                        evidence_ids=evidence_ids,
                        recommendation="Repair the failure or collect fresh verification evidence before final acceptance.",
                        blocking=True,
                        metadata={"check_id": failed.get("check_id")},
                    )
                )
        elif status == "needs_review":
            findings.append(
                ReviewFinding(
                    title="Verification requires review",
                    severity=ReviewSeverity.ERROR,
                    category=ReviewCategory.VERIFICATION_GAP,
                    evidence=[str(item) for item in (assessment.get("remaining_risks") or assessment.get("warnings") or ["Verification assessment needs review."])],
                    evidence_ids=evidence_ids,
                    recommendation="Resolve remaining verification risks or ask for human approval.",
                    blocking=True,
                )
            )
        for risk in assessment.get("remaining_risks") or []:
            findings.append(
                ReviewFinding(
                    title="Verification remaining risk",
                    severity=ReviewSeverity.WARNING,
                    category=ReviewCategory.VERIFICATION_GAP,
                    evidence=[str(risk)],
                    evidence_ids=evidence_ids,
                    recommendation="Document or reduce the remaining risk before finalizing.",
                    blocking=False,
                )
            )
        return findings

    def _from_policy(self, evidence: list[ReviewEvidence], context: dict[str, Any]) -> list[ReviewFinding]:
        policy = _dict(context.get("policy_observation"))
        if not policy:
            return []
        outcome = str(policy.get("outcome") or "").lower()
        if outcome not in {"deny", "require_review", "sandbox_required", "ask_user", "escalate"}:
            return []
        return [
            ReviewFinding(
                title="Policy risk requires review",
                severity=ReviewSeverity.CRITICAL if outcome == "deny" else ReviewSeverity.ERROR,
                category=ReviewCategory.POLICY_RISK,
                evidence=[str(policy.get("reason") or f"Policy outcome={outcome}.")],
                evidence_ids=_evidence_ids(evidence, {"policy_observation"}),
                recommendation="Stop automation and obtain policy or human approval before continuing.",
                blocking=True,
                metadata={"policy_outcome": outcome, "decision_id": policy.get("decision_id")},
            )
        ]

    def _from_final_gap(self, target: ReviewTarget, context: dict[str, Any]) -> list[ReviewFinding]:
        if target.stage != ReviewStage.FINAL:
            return []
        verification = _dict(context.get("verification"))
        status = str((_dict(verification.get("completion_assessment")).get("status") if verification else "") or "").lower()
        if status in {"ready", "ready_with_warnings"}:
            return []
        return [
            ReviewFinding(
                title="Final review lacks passing verification evidence",
                severity=ReviewSeverity.ERROR,
                category=ReviewCategory.VERIFICATION_GAP,
                evidence=[f"Final review verification status={status or 'not_run'}."],
                recommendation="Run verification or replan before final acceptance.",
                blocking=True,
            )
        ]


def _dict(value: Any) -> dict[str, Any]:
    plain = to_bounded_plain(value)
    return plain if isinstance(plain, dict) else {}


def _issue_summary(issue: Any) -> str:
    item = _dict(issue)
    if not item:
        return str(issue)
    return f"{item.get('code') or item.get('category') or 'issue'}: {item.get('message') or item.get('details') or ''}".strip()


def _verification_summary(result: dict[str, Any]) -> str:
    failure = result.get("failure_type")
    check_id = result.get("check_id")
    status = result.get("status")
    kind = result.get("kind")
    evidence = result.get("evidence") if isinstance(result.get("evidence"), dict) else {}
    output = evidence.get("output_excerpt")
    summary = f"check_id={check_id} kind={kind} status={status} failure_type={failure}"
    return f"{summary}: {output}" if output else summary


def _evidence_ids(evidence: list[ReviewEvidence], sources: set[str]) -> list[str]:
    return [item.evidence_id for item in evidence if item.source in sources]


def _dedupe_findings(findings: list[ReviewFinding]) -> list[ReviewFinding]:
    seen: set[tuple[str, str, str]] = set()
    unique: list[ReviewFinding] = []
    for finding in findings:
        key = (finding.title, finding.category.value, "|".join(finding.evidence))
        if key in seen:
            continue
        seen.add(key)
        unique.append(finding)
    return unique
