from __future__ import annotations

from singularity.review import (
    ReviewCategory,
    ReviewDecision,
    ReviewDecisionAction,
    ReviewEvidence,
    ReviewFinding,
    ReviewReport,
    ReviewSeverity,
    ReviewStage,
    ReviewTarget,
)


def test_finding_model_supports_required_fields_and_serialization() -> None:
    finding = ReviewFinding(
        title="Validation requires review",
        severity=ReviewSeverity.ERROR,
        category=ReviewCategory.OVER_EDITING,
        location={"path": "src/app.py", "lines": "1-5", "symbol": "main"},
        evidence=["Large diff touched the entrypoint."],
        recommendation="Narrow the patch before applying.",
        blocking=True,
    )

    payload = finding.model_dump(mode="json")

    assert payload["severity"] == "error"
    assert payload["category"] == "over_editing"
    assert payload["location"]["path"] == "src/app.py"
    assert payload["blocking"] is True
    assert payload["evidence"] == ["Large diff touched the entrypoint."]


def test_review_report_is_machine_readable() -> None:
    target = ReviewTarget(stage=ReviewStage.PRE_EDIT, task_id="task_1", patch_id="patch_1")
    evidence = ReviewEvidence(
        source="patch_validation",
        summary="Patch validation requires review.",
        artifact_ref="patch:patch_1",
        payload_hash="abc",
    )
    finding = ReviewFinding(
        title="Patch validation requires review",
        severity="error",
        category="over_editing",
        evidence=["Patch validation marked requires_review."],
        recommendation="Repair or narrow the edit.",
        blocking=True,
    )
    decision = ReviewDecision(
        action=ReviewDecisionAction.REPAIR,
        reasons=["Blocking edit finding."],
        finding_ids=[finding.finding_id],
    )

    report = ReviewReport(
        target=target,
        input_summary="pre edit bundle",
        evidence=[evidence],
        findings=[finding],
        decision=decision,
        next_actions=["repair"],
    )
    round_trip = ReviewReport.model_validate(report.model_dump(mode="json"))

    assert round_trip.target.stage == ReviewStage.PRE_EDIT
    assert round_trip.findings[0].category == ReviewCategory.OVER_EDITING
    assert round_trip.decision.action == ReviewDecisionAction.REPAIR
