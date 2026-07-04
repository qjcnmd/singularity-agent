from __future__ import annotations

from singularity.verification.models import (
    CheckStatus,
    CompletionAssessment,
    CompletionStatus,
    VerificationPlan,
    VerificationResult,
)

REQUIRED_FAILURE_CONFIDENCE_PENALTY = 0.3
REQUIRED_BLOCKED_CONFIDENCE_PENALTY = 0.25
REQUIRED_FLAKY_CONFIDENCE_PENALTY = 0.15
OPTIONAL_FAILURE_CONFIDENCE_PENALTY = 0.1
REQUIRED_MISSING_CONFIDENCE_PENALTY = 0.1
MANUAL_REVIEW_CONFIDENCE_PENALTY = 0.2


class CompletionAssessor:
    def assess(
        self,
        *,
        plan: VerificationPlan,
        results: list[VerificationResult],
    ) -> CompletionAssessment:
        result_by_check = {result.check_id: result for result in results}
        required_ids = {check.id for check in plan.required_checks}
        optional_ids = {check.id for check in plan.optional_checks}
        passed = [
            result.check_id
            for result in results
            if result.status == CheckStatus.PASSED
        ]
        failed = [
            result.check_id
            for result in results
            if result.status in {CheckStatus.FAILED, CheckStatus.TIMEOUT}
        ]
        blocked = [
            result.check_id
            for result in results
            if result.status == CheckStatus.BLOCKED
        ]
        flaky = [
            result.check_id
            for result in results
            if result.status == CheckStatus.FLAKY
        ]
        skipped = [check.id for check in plan.skipped_checks]
        skipped.extend(
            check.id for check in plan.required_checks if check.id not in result_by_check
        )

        warnings: list[str] = []
        risks = list(plan.impact_analysis.risk_reasons)
        if plan.blocked_checks:
            warnings.append("Some checks were blocked by verification policy or missing commands.")
            risks.extend(check.skip_reason or "Blocked check." for check in plan.blocked_checks)
        if flaky:
            warnings.append("At least one check produced inconsistent results and is marked flaky.")
            risks.append("Flaky verification lowers completion confidence.")
        if any(check_id in optional_ids for check_id in failed):
            warnings.append("Optional verification checks failed.")
        if plan.impact_analysis.requires_manual_review:
            warnings.append("The change requires manual review because it touched high-risk files.")

        required_failed = [check_id for check_id in failed if check_id in required_ids]
        required_blocked = [check_id for check_id in blocked if check_id in required_ids]
        required_flaky = [check_id for check_id in flaky if check_id in required_ids]
        required_missing = [
            check.id for check in plan.required_checks if check.id not in result_by_check
        ]

        if required_failed:
            status = CompletionStatus.FAILED
        elif required_blocked or plan.blocked_checks:
            status = CompletionStatus.BLOCKED
        elif required_missing or plan.impact_analysis.requires_manual_review:
            status = CompletionStatus.NEEDS_REVIEW
        elif warnings or required_flaky:
            status = CompletionStatus.READY_WITH_WARNINGS
        else:
            status = CompletionStatus.READY

        confidence = 1.0
        confidence -= REQUIRED_FAILURE_CONFIDENCE_PENALTY * len(required_failed)
        confidence -= REQUIRED_BLOCKED_CONFIDENCE_PENALTY * len(required_blocked)
        confidence -= REQUIRED_FLAKY_CONFIDENCE_PENALTY * len(required_flaky)
        confidence -= OPTIONAL_FAILURE_CONFIDENCE_PENALTY * len([check_id for check_id in failed if check_id in optional_ids])
        confidence -= REQUIRED_MISSING_CONFIDENCE_PENALTY * len(required_missing)
        if plan.impact_analysis.requires_manual_review:
            confidence -= MANUAL_REVIEW_CONFIDENCE_PENALTY
        confidence = max(0.0, min(1.0, confidence))

        return CompletionAssessment(
            status=status,
            confidence=round(confidence, 2),
            passed_checks=passed,
            failed_checks=failed + blocked + flaky,
            skipped_checks=skipped,
            warnings=warnings,
            remaining_risks=sorted(set(risks)),
        )
