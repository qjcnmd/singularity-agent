from __future__ import annotations

from typing import Any
from uuid import uuid4

from singularity.execution_outcome import ExecutionOutcome, ExecutionOutcomeStatus
from singularity.failure_analysis.result import FailureAnalysisResult
from singularity.failure_analysis.request import FailureAnalysisRequest

from .contract import BLOCKED_FAILURE_CATEGORIES, RepairActionCandidate, RepairContract
from .plan import RepairPlan
from .signal import RepairReplanSignal


class RepairPlanner:
    blocked_categories = BLOCKED_FAILURE_CATEGORIES

    def __init__(self, *, trace: Any | None = None) -> None:
        self.trace = trace

    def plan(
        self,
        analysis: FailureAnalysisResult,
        *,
        repair_policy: dict[str, Any] | None = None,
    ) -> RepairPlan:
        blocked = (
            analysis.blocked_reason
            or (
                analysis.failure_category
                if analysis.failure_category in self.blocked_categories
                else None
            )
        )
        if blocked or analysis.needs_user_input:
            contract = RepairContract.blocked(analysis, reason=blocked or "user_input_required")
            self._record_contract_validation(contract)
            return RepairPlan(
                plan_id=f"repair_{uuid4().hex[:12]}",
                analysis_id=analysis.analysis_id,
                strategy="blocked",
                summary=analysis.repair_strategy or analysis.root_cause,
                action_candidates=[],
                next_actions=analysis.next_actions,
                verification_plan=analysis.verification_plan,
                evidence_refs=analysis.evidence_refs,
                confidence=analysis.confidence,
                needs_user_input=True,
                blocked_reason=blocked or "user_input_required",
                repair_contract=contract,
                verification_contract=contract.verification_contract,
            )
        candidates = _action_candidates(analysis)
        if repair_policy is not None:
            allowed = set(repair_policy.get("allowed_repair_actions") or [])
            if allowed:
                candidates = [
                    c for c in candidates
                    if not hasattr(c, "action_type") or c.action_type in allowed
                    or str(getattr(c, "action_type", "")) in allowed
                ]
            escalation_threshold = repair_policy.get("escalation_threshold")
            if isinstance(escalation_threshold, int) and escalation_threshold <= 0:
                contract = RepairContract.blocked(
                    analysis, reason="repair_policy_escalation_threshold_reached"
                )
                self._record_contract_validation(contract)
                return RepairPlan(
                    plan_id=f"repair_{uuid4().hex[:12]}",
                    analysis_id=analysis.analysis_id,
                    strategy="blocked",
                    summary=analysis.root_cause,
                    action_candidates=[],
                    next_actions=analysis.next_actions,
                    verification_plan=analysis.verification_plan,
                    evidence_refs=analysis.evidence_refs,
                    confidence=analysis.confidence,
                    needs_user_input=True,
                    blocked_reason="repair_policy_escalation_threshold_reached",
                    repair_contract=contract,
                    verification_contract=contract.verification_contract,
                )
        contract = RepairContract.from_analysis(analysis, action_candidates=candidates)
        self._record_contract_validation(contract)
        if contract.needs_user_input or contract.blocked_reason:
            return RepairPlan(
                plan_id=f"repair_{uuid4().hex[:12]}",
                analysis_id=analysis.analysis_id,
                strategy="blocked",
                summary=analysis.root_cause,
                action_candidates=[],
                next_actions=analysis.next_actions,
                verification_plan=analysis.verification_plan,
                evidence_refs=analysis.evidence_refs,
                confidence=analysis.confidence,
                needs_user_input=True,
                blocked_reason=contract.blocked_reason or "repair_contract_invalid",
                repair_contract=contract,
                verification_contract=contract.verification_contract,
            )
        return RepairPlan(
            plan_id=f"repair_{uuid4().hex[:12]}",
            analysis_id=analysis.analysis_id,
            strategy=analysis.repair_strategy or "repair_then_verify",
            summary=analysis.root_cause,
            action_candidates=candidates,
            next_actions=analysis.next_actions,
            verification_plan=analysis.verification_plan,
            evidence_refs=analysis.evidence_refs,
            confidence=analysis.confidence,
            repair_contract=contract,
            verification_contract=contract.verification_contract,
        )

    def to_replan_signal(
        self,
        *,
        request: FailureAnalysisRequest,
        analysis: FailureAnalysisResult,
        plan: RepairPlan,
    ) -> RepairReplanSignal:
        contract = plan.repair_contract or RepairContract.from_analysis(
            analysis,
            action_candidates=plan.action_candidates,
        )
        return RepairReplanSignal.from_contract(
            request=request,
            analysis=analysis,
            plan=plan,
            contract=contract,
        )

    @staticmethod
    def blocked_outcome(plan: RepairPlan) -> ExecutionOutcome:
        return ExecutionOutcome(
            status=ExecutionOutcomeStatus.USER_INPUT_REQUIRED,
            source="failure_analysis",
            reason=plan.blocked_reason or "failure_analysis_requires_user_input",
            error_code="failure_analysis_user_input_required",
            next_action="ask_user",
            observation_summary=plan.summary,
            retry_allowed=False,
            metadata={"repair_plan": plan.to_dict()},
        )

    def _record_contract_validation(self, contract: RepairContract) -> None:
        if self.trace is None or not hasattr(self.trace, "record"):
            return
        self.trace.record(
            "repair_contract_validation",
            {
                "contract_id": contract.contract_id,
                "analysis_id": contract.analysis_id,
                "valid": not contract.validation_errors and not contract.blocked_reason,
                "validation_errors": contract.validation_errors,
                "needs_user_input": contract.needs_user_input,
                "blocked_reason": contract.blocked_reason,
            },
        )

def _action_candidates(analysis: FailureAnalysisResult) -> list[RepairActionCandidate]:
    actions = analysis.next_actions or [analysis.repair_strategy]
    files: list[str | None] = list(analysis.affected_files) or [None]
    candidates: list[RepairActionCandidate] = []
    for index, action in enumerate(actions[:6]):
        lowered = action.lower()
        action_type = "analyze"
        tool_hints = ["read_file", "search_text"]
        if any(marker in lowered for marker in ("patch", "edit", "fix", "repair", "修改", "修复")):
            action_type = "edit"
            tool_hints = ["read_file", "apply_patch", "write_file", "inspect_diff"]
        elif any(marker in lowered for marker in ("verify", "rerun", "test", "pytest", "验证", "测试")):
            action_type = "verify"
            tool_hints = ["run_verification", "get_verification_result"]
        elif any(marker in lowered for marker in ("read", "inspect", "open", "查", "读")):
            action_type = "inspect"
        candidates.append(
            RepairActionCandidate(
                candidate_id=f"candidate_{uuid4().hex[:12]}",
                action_type=action_type,
                target_file=files[min(index, len(files) - 1)],
                rationale=action,
                tool_hints=tool_hints,
                verification_ref=analysis.verification_plan[0] if analysis.verification_plan else None,
                confidence=analysis.confidence,
            )
        )
    return candidates
