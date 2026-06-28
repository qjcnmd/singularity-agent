from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any
from uuid import uuid4

from singularity.failure_analysis.request import FailureAnalysisRequest
from singularity.failure_analysis.result import FailureAnalysisResult
from singularity.verification.contract import VerificationContract

from .contract import RepairContract
from .plan import RepairPlan


@dataclass(frozen=True)
class RepairReplanSignal:
    signal_id: str
    repair_plan_id: str
    analysis_id: str
    contract_id: str
    failure_fingerprint: str
    failure_category: str
    target_files: list[str]
    action_candidates: list[dict[str, Any]]
    verification_plan: list[str]
    confidence: float
    needs_user_input: bool
    blocked_reason: str | None
    repair_contract: RepairContract
    error_code: str
    verification_failed: bool = True
    verification_contract: VerificationContract = field(
        default_factory=VerificationContract.empty
    )

    @classmethod
    def from_contract(
        cls,
        *,
        request: FailureAnalysisRequest,
        analysis: FailureAnalysisResult,
        plan: RepairPlan,
        contract: RepairContract,
    ) -> RepairReplanSignal:
        return cls(
            signal_id=f"repair_signal_{uuid4().hex[:12]}",
            repair_plan_id=plan.plan_id,
            analysis_id=analysis.analysis_id,
            contract_id=contract.contract_id,
            failure_fingerprint=request.fingerprint,
            failure_category=analysis.failure_category,
            target_files=list(contract.target_files),
            action_candidates=[item.to_dict() for item in contract.action_candidates],
            verification_plan=list(contract.verification_plan),
            confidence=contract.confidence,
            needs_user_input=contract.needs_user_input,
            blocked_reason=contract.blocked_reason,
            repair_contract=contract,
            error_code=analysis.failure_category or "repair_planned",
            verification_contract=contract.verification_contract,
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "signal_id": self.signal_id,
            "repair_plan_id": self.repair_plan_id,
            "analysis_id": self.analysis_id,
            "contract_id": self.contract_id,
            "failure_fingerprint": self.failure_fingerprint,
            "failure_category": self.failure_category,
            "target_files": self.target_files,
            "action_candidates": self.action_candidates,
            "verification_plan": self.verification_plan,
            "verification_contract": self.verification_contract.to_dict(),
            "confidence": self.confidence,
            "needs_user_input": self.needs_user_input,
            "blocked_reason": self.blocked_reason,
            "repair_contract": self.repair_contract.to_dict(),
            "error_code": self.error_code,
            "verification_failed": self.verification_failed,
        }
