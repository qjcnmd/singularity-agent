"""Tests for the Final Reviewer Runtime — per-criterion completion gate.

Covers:
- FinalReviewer.assess: pass when all criteria satisfied
- FinalReviewer.assess: block when evidence missing
- FinalReviewer.assess: block when verification failed
- FinalReviewer.assess: report risk_remaining
- FinalReviewer.assess: fallback when no SemanticPlan
- FinalReviewer.assess: model can confirm with evidence_refs
- FinalReviewer.assess: model cannot override evidence gate (failed_evidence)
- CompletionAssessment / CriterionAssessment round-trip
- EvidenceLedger.query_evidence / evidence_for_criterion
"""

from __future__ import annotations

import json
from typing import Any

from singularity.model.models import (
    ModelMessage,
    ModelPurpose,
    ModelTurnRequest,
    ModelTurnResult,
    ModelTurnStatus,
)
from singularity.planner.contract import AcceptanceCriterion, TaskContract
from singularity.planner.final_reviewer import (
    CompletionAssessment,
    CriterionAssessment,
    FinalReviewer,
)
from singularity.planner.models import EvidenceLedger, TaskState
from singularity.planner.semantic import PlanStep, RollingPlan
from singularity.planner.semantic_objects import (
    RepairPolicy,
    RiskPoint,
    SemanticPlan,
    VerificationStrategy,
)

# ---------------------------------------------------------------------------
# Fakes (same pattern as test_semantic_planner_capability.py)
# ---------------------------------------------------------------------------


class FakeModelRunner:
    """Duck-typed fake that returns scripted JSON per ModelPurpose."""

    def __init__(self, responses: dict[Any, Any]) -> None:
        self.responses = responses
        self.requests: list[ModelTurnRequest] = []

    def run_turn(self, request: ModelTurnRequest) -> ModelTurnResult:
        self.requests.append(request)
        value = self.responses.get(request.purpose)
        if isinstance(value, Exception):
            raise value
        if value is None:
            return ModelTurnResult(
                request_id=request.request_id,
                response_id="resp_fake",
                status=ModelTurnStatus.FAILED,
            )
        return ModelTurnResult(
            request_id=request.request_id,
            response_id="resp_fake",
            status=ModelTurnStatus.SUCCESS,
            assistant_message=ModelMessage.assistant_text(value),
        )


class FakeTrace:
    def __init__(self) -> None:
        self.events: list[tuple[str, dict[str, Any]]] = []

    def emit(
        self,
        event: str,
        *,
        component: str = "",
        summary: str = "",
        payload: dict[str, Any] | None = None,
        ids: dict[str, Any] | None = None,
        severity: str = "info",
    ) -> None:
        self.events.append(
            (event, {"summary": summary, "payload": payload or {}, "severity": severity})
        )

    def record(self, event: str, payload: dict[str, Any] | None = None) -> None:
        self.events.append((event, payload or {}))

    def has_event(self, name: str) -> bool:
        return any(name == e for e, _ in self.events)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _make_contract(
    criteria: list[AcceptanceCriterion] | None = None,
) -> TaskContract:
    if criteria is None:
        criteria = [
            AcceptanceCriterion(
                criterion_id="c1",
                description="Changes applied",
                evidence=["applied_changes"],
                required=True,
            ),
            AcceptanceCriterion(
                criterion_id="c2",
                description="Verification passed",
                evidence=["verification_results"],
                required=True,
            ),
        ]
    return TaskContract(
        user_goal="test goal",
        acceptance_criteria=criteria,
    )


def _make_state(
    *,
    verification_status: str | None = None,
) -> TaskState:
    state = TaskState(
        task_id="t1",
        session_id="s1",
        user_goal="test goal",
        normalized_goal="test goal",
    )
    state.final_assessment = {"status": verification_status} if verification_status else {}
    return state


def _make_plan(
    *,
    risk_points: list[RiskPoint] | None = None,
    verification_strategies: list[VerificationStrategy] | None = None,
    repair_policy: RepairPolicy | None = None,
) -> SemanticPlan:
    step = PlanStep(step_id="s1", title="step", kind="action")
    return SemanticPlan(
        rolling_plan=RollingPlan(
            plan_id="p1",
            user_goal="test goal",
            current_step_id="s1",
            steps=[step],
        ),
        risk_points=risk_points or [],
        verification_strategies=verification_strategies or [],
        repair_policy=repair_policy,
        producer_source="rules",
    )


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


def test_final_reviewer_passes_when_all_criteria_satisfied() -> None:
    contract = _make_contract()
    evidence = EvidenceLedger()
    evidence.applied_changes.append({"file": "a.py", "change": "fix"})
    evidence.verification_results.append({"check_id": "v1", "status": "passed"})
    state = _make_state(verification_status="ready")
    reviewer = FinalReviewer()
    assessment = reviewer.assess(
        contract=contract, plan=None, evidence=evidence, state=state
    )
    assert assessment.overall_satisfied
    assert not assessment.blocking_reasons
    assert all(c.satisfied for c in assessment.criteria)


def test_final_reviewer_blocks_when_evidence_missing() -> None:
    contract = _make_contract()
    evidence = EvidenceLedger()
    # applied_changes present but verification_results empty
    evidence.applied_changes.append({"file": "a.py"})
    state = _make_state(verification_status="ready")
    reviewer = FinalReviewer()
    assessment = reviewer.assess(
        contract=contract, plan=None, evidence=evidence, state=state
    )
    assert not assessment.overall_satisfied
    assert any("c2" in reason for reason in assessment.blocking_reasons)
    c2 = assessment.criterion("c2")
    assert c2 is not None
    assert "verification_results" in c2.missing_evidence


def test_final_reviewer_blocks_when_verification_failed() -> None:
    contract = _make_contract()
    evidence = EvidenceLedger()
    evidence.applied_changes.append({"file": "a.py"})
    evidence.verification_results.append({"check_id": "v1", "status": "failed"})
    state = _make_state(verification_status="failed")
    reviewer = FinalReviewer()
    assessment = reviewer.assess(
        contract=contract, plan=None, evidence=evidence, state=state
    )
    assert not assessment.overall_satisfied
    c2 = assessment.criterion("c2")
    assert c2 is not None
    assert "verification_results" in c2.failed_evidence


def test_final_reviewer_reports_risk_remaining() -> None:
    risk = RiskPoint(
        risk_id="r1",
        description="test breakage",
        trigger_conditions=["condition"],
        mitigation_strategy="add test",
        severity="high",
        acceptance_criterion_id="c1",
    )
    contract = _make_contract()
    evidence = EvidenceLedger()
    evidence.applied_changes.append({"file": "a.py"})
    evidence.verification_results.append({"check_id": "v1", "status": "passed"})
    state = _make_state(verification_status="ready")
    # No command_results → mitigation not evidenced → risk_remaining
    plan = _make_plan(risk_points=[risk])
    reviewer = FinalReviewer()
    assessment = reviewer.assess(
        contract=contract, plan=plan, evidence=evidence, state=state
    )
    c1 = assessment.criterion("c1")
    assert c1 is not None
    assert "r1" in c1.risk_remaining
    assert not c1.satisfied
    assert not assessment.overall_satisfied


def test_final_reviewer_fallback_when_no_semantic_plan() -> None:
    contract = _make_contract()
    evidence = EvidenceLedger()
    evidence.applied_changes.append({"file": "a.py"})
    evidence.verification_results.append({"check_id": "v1", "status": "passed"})
    state = _make_state(verification_status="ready")
    reviewer = FinalReviewer()
    assessment = reviewer.assess(
        contract=contract, plan=None, evidence=evidence, state=state
    )
    assert assessment.overall_satisfied
    assert assessment.producer_source == "rules"


def test_final_reviewer_model_can_confirm_with_evidence_refs() -> None:
    """Model can confirm a criterion as satisfied when the evidence_key is
    missing from the ledger (not failed — model supplies evidence_refs)."""
    contract = _make_contract(
        criteria=[
            AcceptanceCriterion(
                criterion_id="c1",
                description="Custom evidence",
                evidence=["custom_evidence"],
                required=True,
            ),
        ]
    )
    evidence = EvidenceLedger()
    # custom_evidence bucket is empty → c1 initially unsatisfied (missing)
    state = _make_state(verification_status="ready")
    model_response = json.dumps(
        {
            "criteria": [
                {
                    "criterion_id": "c1",
                    "satisfied": True,
                    "evidence_refs": ["custom_evidence:1"],
                },
            ]
        }
    )
    runner = FakeModelRunner({ModelPurpose.FINAL_REVIEW: model_response})
    reviewer = FinalReviewer(model_runner=runner)
    assessment = reviewer.assess(
        contract=contract, plan=None, evidence=evidence, state=state
    )
    # c1 should have been confirmed by the model (missing → satisfied with refs)
    assert assessment.overall_satisfied
    assert any(c.producer_source == "model" for c in assessment.criteria)


def test_final_reviewer_model_cannot_override_evidence_gate() -> None:
    """Model says satisfied=True but criterion has failed_evidence → stays False."""
    contract = _make_contract()
    evidence = EvidenceLedger()
    evidence.applied_changes.append({"file": "a.py"})
    evidence.verification_results.append({"check_id": "v1", "status": "failed"})
    state = _make_state(verification_status="failed")
    model_response = json.dumps(
        {
            "criteria": [
                {
                    "criterion_id": "c1",
                    "satisfied": True,
                    "evidence_refs": ["applied_changes:1"],
                },
                {
                    "criterion_id": "c2",
                    "satisfied": True,
                    "evidence_refs": ["verification_results:1"],
                },
            ]
        }
    )
    runner = FakeModelRunner({ModelPurpose.FINAL_REVIEW: model_response})
    reviewer = FinalReviewer(model_runner=runner)
    assessment = reviewer.assess(
        contract=contract, plan=None, evidence=evidence, state=state
    )
    c2 = assessment.criterion("c2")
    assert c2 is not None
    assert not c2.satisfied
    assert "verification_results" in c2.failed_evidence
    assert not assessment.overall_satisfied


def test_completion_assessment_round_trip() -> None:
    original = CompletionAssessment(
        overall_satisfied=False,
        criteria=[
            CriterionAssessment(
                criterion_id="c1",
                description="desc",
                required=True,
                satisfied=False,
                missing_evidence=["applied_changes"],
                failed_evidence=[],
                risk_remaining=["r1"],
                evidence_refs=["applied_changes:0"],
                producer_source="rules",
            )
        ],
        blocking_reasons=["criterion:c1:missing=applied_changes"],
        producer_source="rules",
    )
    payload = original.to_dict()
    restored = CompletionAssessment.from_dict(payload)
    assert restored.overall_satisfied == original.overall_satisfied
    assert len(restored.criteria) == 1
    assert restored.criteria[0].criterion_id == "c1"
    assert restored.criteria[0].missing_evidence == ["applied_changes"]
    assert restored.criteria[0].risk_remaining == ["r1"]
    assert restored.blocking_reasons == original.blocking_reasons


def test_evidence_ledger_query_evidence() -> None:
    ledger = EvidenceLedger()
    ledger.inspected_files.append("a.py")
    ledger.inspected_files.append("b.py")
    ledger.applied_changes.append({"file": "c.py"})
    assert len(ledger.query_evidence("inspected_files")) == 2
    assert len(ledger.query_evidence("applied_changes")) == 1
    assert ledger.query_evidence("nonexistent_key") == []


def test_evidence_ledger_evidence_for_criterion() -> None:
    ledger = EvidenceLedger()
    ledger.applied_changes.append({"file": "a.py", "criterion_id": "c1"})
    ledger.applied_changes.append({"file": "b.py", "criterion_id": "c2"})
    ledger.command_results.append({"command": "pytest", "criterion_id": "c1"})
    results = ledger.evidence_for_criterion("c1")
    assert len(results) == 2
    assert all(r.get("criterion_id") == "c1" for r in results)
