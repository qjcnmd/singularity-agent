"""Final Reviewer Runtime — per-criterion completion gate.

The ``FinalReviewer`` is the gate that runs before ``Planner.finalize()``
marks a task ``COMPLETED``. Unlike ``Finalizer.build`` (which only reads the
latest review decision + verification status), the FinalReviewer walks every
``TaskContract.acceptance_criteria`` entry and checks it against
``SemanticPlan.verification_strategies`` + ``EvidenceLedger`` evidence.

Model-visible vs internal trace/debug boundary:
- Model-visible: ``CompletionAssessment.criteria`` summaries (criterion_id /
  satisfied / missing_evidence) are projected into the main task model context
  via ``PlannerContextRenderer``.
- Internal trace/debug: full ``CriterionAssessment`` (with failed_evidence /
  risk_remaining / evidence_refs) + ``final_reviewer.assess.{model_ok|fallback}``
  trace events.

Model participation is opt-in and non-overriding: when ``model_runner`` is
provided, the model can *confirm* a criterion as satisfied (attaching
``evidence_refs``), but it CANNOT override an evidence-gate failure — a
criterion whose evidence bucket is empty or whose verification status is
``failed`` stays ``satisfied=False`` regardless of what the model says. This
preserves the fail-closed guarantee.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import Any
from uuid import uuid4

from pydantic import BaseModel, ConfigDict, Field

from singularity.model.models import ModelPurpose
from singularity.planner.contract import TaskContract
from singularity.planner.models import EvidenceLedger, TaskState
from singularity.planner.semantic_objects import (
    RiskPoint,
    SemanticPlan,
    VerificationStrategy,
)
from singularity.review.structured_output import (
    BusinessRuleViolation,
    ReviewOutputResult,
    call_review_output,
)


@dataclass(frozen=True)
class CriterionAssessment:
    """Per-criterion assessment produced by ``FinalReviewer.assess``.

    ``satisfied`` is the authoritative gate value — it can only be ``True``
    when evidence exists (or the model confirmed with ``evidence_refs``).
    ``missing_evidence`` lists evidence_keys that have no records;
    ``failed_evidence`` lists evidence_keys whose records indicate failure;
    ``risk_remaining`` lists ``risk_id`` values of unresolved ``RiskPoint``s
    bound to this criterion; ``evidence_refs`` are stable refs (bucket:index)
    into ``EvidenceLedger``.
    """

    criterion_id: str
    description: str
    required: bool
    satisfied: bool
    missing_evidence: list[str] = field(default_factory=list)
    failed_evidence: list[str] = field(default_factory=list)
    risk_remaining: list[str] = field(default_factory=list)
    evidence_refs: list[str] = field(default_factory=list)
    producer_source: str = "rules"

    def to_dict(self) -> dict[str, Any]:
        return {
            "criterion_id": self.criterion_id,
            "description": self.description,
            "required": self.required,
            "satisfied": self.satisfied,
            "missing_evidence": list(self.missing_evidence),
            "failed_evidence": list(self.failed_evidence),
            "risk_remaining": list(self.risk_remaining),
            "evidence_refs": list(self.evidence_refs),
            "producer_source": self.producer_source,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> CriterionAssessment:
        return cls(
            criterion_id=str(payload["criterion_id"]),
            description=str(payload.get("description") or payload["criterion_id"]),
            required=bool(payload.get("required", True)),
            satisfied=bool(payload.get("satisfied", False)),
            missing_evidence=[str(item) for item in payload.get("missing_evidence") or []],
            failed_evidence=[str(item) for item in payload.get("failed_evidence") or []],
            risk_remaining=[str(item) for item in payload.get("risk_remaining") or []],
            evidence_refs=[str(item) for item in payload.get("evidence_refs") or []],
            producer_source=str(payload.get("producer_source") or "rules"),
        )


@dataclass(frozen=True)
class CompletionAssessment:
    """Overall completion assessment aggregating per-criterion results.

    ``overall_satisfied`` is ``True`` only when every *required* criterion is
    ``satisfied``. ``blocking_reasons`` are human-readable strings explaining
    why completion is blocked (one per unsatisfied required criterion).
    """

    overall_satisfied: bool
    criteria: list[CriterionAssessment] = field(default_factory=list)
    blocking_reasons: list[str] = field(default_factory=list)
    producer_source: str = "rules"

    def to_dict(self) -> dict[str, Any]:
        return {
            "overall_satisfied": self.overall_satisfied,
            "criteria": [item.to_dict() for item in self.criteria],
            "blocking_reasons": list(self.blocking_reasons),
            "producer_source": self.producer_source,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> CompletionAssessment:
        return cls(
            overall_satisfied=bool(payload.get("overall_satisfied", False)),
            criteria=[
                CriterionAssessment.from_dict(item)
                for item in payload.get("criteria") or []
            ],
            blocking_reasons=[str(item) for item in payload.get("blocking_reasons") or []],
            producer_source=str(payload.get("producer_source") or "rules"),
        )

    def criterion(self, criterion_id: str) -> CriterionAssessment | None:
        for item in self.criteria:
            if item.criterion_id == criterion_id:
                return item
        return None


class FinalReviewCriterionOutput(BaseModel):
    model_config = ConfigDict(extra="forbid")

    criterion_id: str
    satisfied: bool = False
    evidence_refs: list[str] = Field(default_factory=list)


class FinalReviewCriteria(BaseModel):
    model_config = ConfigDict(extra="forbid")

    criteria: list[FinalReviewCriterionOutput] = Field(default_factory=list)


def _needs_model_confirmation(criteria: list[CriterionAssessment]) -> bool:
    return any(criterion.required and not criterion.satisfied for criterion in criteria)


def _emit(
    trace: Any,
    event: str,
    *,
    summary: str,
    payload: dict[str, Any] | None = None,
    ids: dict[str, Any] | None = None,
    severity: str = "info",
) -> None:
    """Emit a trace event, tolerating recorders without ``emit``."""
    if trace is None:
        return
    if hasattr(trace, "emit"):
        trace.emit(
            event,
            component="final_reviewer",
            summary=summary,
            payload=payload or {},
            ids=ids or {},
            severity=severity,
        )
    elif hasattr(trace, "record"):
        trace.record(event, {**(payload or {}), "summary": summary, **(ids or {})})


class FinalReviewer:
    """Per-criterion completion gate consuming SemanticPlan + EvidenceLedger.

    Routing:
    1. Build ``CriterionAssessment`` per ``TaskContract.acceptance_criteria``.
    2. For each criterion, check every ``evidence`` key against
       ``EvidenceLedger.query_evidence`` (bucket non-empty) and, when the
       key is ``verification_results``, require the latest
       ``CompletionAssessment.status`` to be ``ready``/``ready_with_warnings``.
    3. Bind ``RiskPoint``s via ``acceptance_criterion_id`` and flag
       ``risk_remaining`` when the risk's mitigation has no corresponding
       applied change or command result.
    4. Optionally call the model-assisted review path
       (``ModelPurpose.FINAL_REVIEW``) through the shared ordered output
       boundary: Structured Outputs / JSON Schema, strict tool calling with
       pinned tool choice, then json_mode. The model can flip ``satisfied``
       True only when it attaches ``evidence_refs``; it cannot downgrade a
       True to False or override failed evidence.
    5. Fail-closed fallback: if no ``SemanticPlan`` is available, fall back to
       the coarse bucket-non-empty check (same as the legacy
       ``Planner._contract_evidence_satisfied``).
    """

    def __init__(
        self,
        *,
        model_runner: Any | None = None,
        trace: Any | None = None,
    ) -> None:
        self.model_runner = model_runner
        self.trace = trace

    def assess(
        self,
        *,
        contract: TaskContract | None,
        plan: SemanticPlan | None,
        evidence: EvidenceLedger,
        state: TaskState,
        context_payload: dict[str, Any] | None = None,
    ) -> CompletionAssessment:
        if contract is None:
            return CompletionAssessment(
                overall_satisfied=True,
                criteria=[],
                blocking_reasons=[],
                producer_source="rules_no_contract",
            )
        risk_points = plan.risk_points if plan is not None else []
        verification_strategies = plan.verification_strategies if plan is not None else []
        criteria: list[CriterionAssessment] = []
        for criterion in contract.acceptance_criteria:
            criteria.append(
                self._assess_criterion(
                    criterion=criterion,
                    evidence=evidence,
                    state=state,
                    risk_points=risk_points,
                    verification_strategies=verification_strategies,
                )
            )
        if self.model_runner is not None and _needs_model_confirmation(criteria):
            criteria = self._model_confirm(criteria, evidence, context_payload or {})
        elif self.model_runner is not None:
            _emit(
                self.trace,
                "final_reviewer.assess.model_skipped",
                summary="final_reviewer model confirm skipped; deterministic evidence gate is decisive",
                payload={
                    "reason": "deterministic_gate_decisive",
                    "criteria_count": len(criteria),
                },
            )
        blocking: list[str] = []
        for item in criteria:
            if item.required and not item.satisfied:
                blocking.append(
                    f"criterion:{item.criterion_id}:missing={','.join(item.missing_evidence) or 'none'},"
                    f"failed={','.join(item.failed_evidence) or 'none'},"
                    f"risk={','.join(item.risk_remaining) or 'none'}"
                )
        overall = not blocking
        producer_source = "model" if (self.model_runner is not None and any(c.producer_source == "model" for c in criteria)) else "rules"
        assessment = CompletionAssessment(
            overall_satisfied=overall,
            criteria=criteria,
            blocking_reasons=blocking,
            producer_source=producer_source,
        )
        _emit(
            self.trace,
            "final_reviewer.assess.done",
            summary=(
                f"final_reviewer overall_satisfied={overall} "
                f"criteria={len(criteria)} blocking={len(blocking)}"
            ),
            payload=assessment.to_dict(),
        )
        return assessment

    def _assess_criterion(
        self,
        *,
        criterion: Any,
        evidence: EvidenceLedger,
        state: TaskState,
        risk_points: list[RiskPoint],
        verification_strategies: list[VerificationStrategy],
    ) -> CriterionAssessment:
        missing: list[str] = []
        failed: list[str] = []
        refs: list[str] = []
        for evidence_key in criterion.evidence:
            records = evidence.query_evidence(evidence_key)
            if not records:
                missing.append(evidence_key)
                continue
            if evidence_key == "verification_results":
                status = (state.final_assessment or {}).get("status")
                if status not in {"ready", "ready_with_warnings"}:
                    failed.append(evidence_key)
                    continue
            refs.append(f"{evidence_key}:{len(records)}")
        strategy = self._strategy_for_criterion(
            criterion.criterion_id, verification_strategies
        )
        if strategy is not None and strategy.evidence_key:
            records = evidence.query_evidence(strategy.evidence_key)
            if not records and strategy.evidence_key not in missing:
                missing.append(strategy.evidence_key)
        risk_remaining = self._risk_remaining_for_criterion(
            criterion.criterion_id, risk_points, evidence
        )
        satisfied = not missing and not failed
        if satisfied and risk_remaining:
            satisfied = False
        return CriterionAssessment(
            criterion_id=criterion.criterion_id,
            description=criterion.description,
            required=criterion.required,
            satisfied=satisfied,
            missing_evidence=missing,
            failed_evidence=failed,
            risk_remaining=risk_remaining,
            evidence_refs=refs,
            producer_source="rules",
        )

    @staticmethod
    def _strategy_for_criterion(
        criterion_id: str,
        strategies: list[VerificationStrategy],
    ) -> VerificationStrategy | None:
        for strategy in strategies:
            if strategy.acceptance_criterion_id == criterion_id:
                return strategy
        return None

    @staticmethod
    def _risk_remaining_for_criterion(
        criterion_id: str,
        risk_points: list[RiskPoint],
        evidence: EvidenceLedger,
    ) -> list[str]:
        remaining: list[str] = []
        for risk in risk_points:
            if risk.acceptance_criterion_id != criterion_id:
                continue
            # Mitigation requires explicit command_results (verification that
            # the mitigation was applied), not just any applied_changes.
            if not evidence.command_results:
                remaining.append(risk.risk_id)
        return remaining

    def _model_confirm(
        self,
        criteria: list[CriterionAssessment],
        evidence: EvidenceLedger,
        context_payload: dict[str, Any],
    ) -> list[CriterionAssessment]:
        """Ask the model to confirm criteria; model can only confirm (not override)."""
        request_ids = _final_review_request_ids(context_payload)
        result = self._call_model(criteria, evidence, request_ids)
        output_metadata = _safe_output_metadata(result.metadata)
        trace_ids = dict(request_ids)
        if result.status != "ok":
            _emit(
                self.trace,
                "final_reviewer.assess.fallback",
                summary="final_reviewer model-assisted review used the rule-only fallback path",
                payload=output_metadata,
                ids=trace_ids,
                severity="warning",
            )
            return criteria
        confirmed: dict[str, dict[str, Any]] = {}
        for item in result.payload.get("criteria") or []:
            cid = item.get("criterion_id")
            if cid:
                confirmed[str(cid)] = item
        updated: list[CriterionAssessment] = []
        for original in criteria:
            entry = confirmed.get(original.criterion_id)
            if not entry:
                updated.append(original)
                continue
            model_satisfied = bool(entry.get("satisfied", False))
            model_refs = [str(r) for r in entry.get("evidence_refs") or []]
            if (
                model_satisfied
                and not original.satisfied
                and not original.failed_evidence
                and model_refs
            ):
                updated.append(
                    CriterionAssessment(
                        criterion_id=original.criterion_id,
                        description=original.description,
                        required=original.required,
                        satisfied=True,
                        missing_evidence=[],
                        failed_evidence=[],
                        risk_remaining=original.risk_remaining,
                        evidence_refs=list(set(original.evidence_refs + model_refs)),
                        producer_source="model",
                    )
                )
                _emit(
                    self.trace,
                    "final_reviewer.assess.model_ok",
                    summary=f"criterion {original.criterion_id} confirmed by model",
                    payload=output_metadata,
                    ids=trace_ids,
                )
            else:
                updated.append(original)
        _emit(
            self.trace,
            "final_reviewer.assess.model_ok",
            summary="final_reviewer model confirm completed",
            payload=output_metadata,
            ids=trace_ids,
        )
        return updated

    def _call_model(
        self,
        criteria: list[CriterionAssessment],
        evidence: EvidenceLedger,
        request_ids: dict[str, str],
    ) -> ReviewOutputResult:
        criteria_json = json.dumps(
            [c.to_dict() for c in criteria], ensure_ascii=False, sort_keys=True, default=str
        )
        evidence_summary = {
            "inspected_files_count": len(evidence.inspected_files),
            "applied_changes_count": len(evidence.applied_changes),
            "command_results_count": len(evidence.command_results),
            "verification_results_count": len(evidence.verification_results),
            "review_results_count": len(evidence.review_results),
            "unresolved_failures_count": len(evidence.unresolved_failures),
        }
        prompt = (
            "You are the Singularity FinalReviewer, a project-internal hard gate. "
            "Use the caller-selected output boundary: Structured Outputs / JSON Schema, "
            "strict tool calling with pinned tool choice, or json_mode fallback. "
            "For each acceptance criterion, decide if it is satisfied based on the evidence summary. "
            "You can ONLY mark a criterion satisfied=True if you attach at least one evidence_ref "
            "(for example 'applied_changes:2'). You CANNOT mark satisfied=True for a criterion that "
            "has failed_evidence or risk_remaining; the local fail-closed deterministic gate is authoritative. "
            "Return a FinalReviewCriteria JSON object with a criteria array.\n\n"
            "Criteria:\n" + criteria_json + "\n\nEvidence summary:\n"
            + json.dumps(evidence_summary, ensure_ascii=False, sort_keys=True)
        )
        runner = self.model_runner
        if runner is None:
            return ReviewOutputResult(
                status="fallback",
                error="model_runner_missing",
                metadata={
                    "output_mode": "rule_only",
                    "schema_validation_passed": False,
                    "retry_count": 0,
                    "retry_reason": "none",
                    "fallback_reason": "model_runner_missing",
                },
            )
        return call_review_output(
            model_runner=runner,
            request_base={
                "request_id": f"req_{uuid4().hex[:12]}",
                "run_id": request_ids["run_id"],
                "session_id": request_ids["session_id"],
                "task_id": request_ids["task_id"],
                "phase_id": request_ids["phase_id"],
                "action_id": request_ids["action_id"],
                "purpose": ModelPurpose.FINAL_REVIEW,
                "context_metadata": {"producer": "final_reviewer"},
                "max_output_tokens": 1200,
            },
            prompt=prompt,
            output_model=FinalReviewCriteria,
            schema_name="final_review_criteria",
            tool_name="submit_final_review",
            tool_description="Submit FinalReviewer criterion confirmations that conform to the FinalReviewCriteria JSON Schema.",
            business_validator=_final_review_business_validator(criteria),
        )


def _final_review_business_validator(criteria: list[CriterionAssessment]) -> Any:
    by_id = {criterion.criterion_id: criterion for criterion in criteria}

    def validate(payload: dict[str, Any]) -> None:
        for entry in payload.get("criteria") or []:
            if not isinstance(entry, dict):
                continue
            criterion_id = str(entry.get("criterion_id") or "")
            original = by_id.get(criterion_id)
            if original is None:
                raise BusinessRuleViolation("model-assisted review referenced an unknown criterion")
            if not bool(entry.get("satisfied", False)):
                continue
            refs = [str(ref) for ref in entry.get("evidence_refs") or []]
            if not refs:
                raise BusinessRuleViolation("model-assisted review confirmation missing evidence_refs")
            if any("evaluator-only" in ref.lower() for ref in refs):
                raise BusinessRuleViolation("model-assisted review referenced evaluator-only evidence")
            if original.failed_evidence or original.risk_remaining:
                raise BusinessRuleViolation(
                    "model-assisted review attempted to override fail-closed gate evidence"
                )

    return validate


def _safe_output_metadata(metadata: dict[str, Any]) -> dict[str, Any]:
    return {
        "output_mode": str(metadata.get("output_mode") or ""),
        "schema_validation_passed": bool(metadata.get("schema_validation_passed")),
        "retry_count": int(metadata.get("retry_count") or 0),
        "retry_reason": str(metadata.get("retry_reason") or "none"),
        "fallback_reason": str(metadata.get("fallback_reason") or ""),
    }


def _final_review_request_ids(context_payload: dict[str, Any]) -> dict[str, str]:
    return {
        "run_id": str(context_payload.get("run_id") or "final_reviewer"),
        "session_id": str(context_payload.get("session_id") or "final_reviewer"),
        "task_id": str(context_payload.get("task_id") or "final_reviewer"),
        "phase_id": str(context_payload.get("phase_id") or "final_reviewer"),
        "action_id": str(context_payload.get("action_id") or uuid4().hex[:12]),
    }
