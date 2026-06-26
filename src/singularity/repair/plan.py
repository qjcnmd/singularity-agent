from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

from singularity.verification.contract import VerificationContract

from .contract import RepairActionCandidate, RepairContract


@dataclass(frozen=True)
class RepairPlan:
    plan_id: str
    analysis_id: str
    strategy: str
    summary: str
    action_candidates: list[RepairActionCandidate]
    next_actions: list[str]
    verification_plan: list[str]
    evidence_refs: list[str]
    confidence: float
    needs_user_input: bool = False
    blocked_reason: str | None = None
    repair_contract: RepairContract | None = None
    verification_contract: VerificationContract = field(
        default_factory=VerificationContract.empty
    )

    def to_dict(self) -> dict[str, Any]:
        return {
            "plan_id": self.plan_id,
            "analysis_id": self.analysis_id,
            "strategy": self.strategy,
            "summary": self.summary,
            "action_candidates": [item.to_dict() for item in self.action_candidates],
            "next_actions": self.next_actions,
            "verification_plan": self.verification_plan,
            "verification_contract": self.verification_contract.to_dict(),
            "evidence_refs": self.evidence_refs,
            "confidence": self.confidence,
            "needs_user_input": self.needs_user_input,
            "blocked_reason": self.blocked_reason,
            "repair_contract": self.repair_contract.to_dict() if self.repair_contract else None,
        }
