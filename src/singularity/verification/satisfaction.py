from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any


@dataclass(frozen=True)
class StepEvidence:
    """Evidence linking a verification step to its execution result."""

    step_id: str
    check_id: str | None
    command_id: str | None
    status: str
    artifact_ref: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "step_id": self.step_id,
            "check_id": self.check_id,
            "command_id": self.command_id,
            "status": self.status,
            "artifact_ref": self.artifact_ref,
        }


@dataclass(frozen=True)
class ContractSatisfaction:
    """Tracks whether a verification contract was satisfied after repair."""

    contract_id: str
    satisfied: bool
    completed_steps: list[str]
    failed_steps: list[str]
    skipped_steps: list[str]
    reason: str | None = None
    step_evidence: list[StepEvidence] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {
            "contract_id": self.contract_id,
            "satisfied": self.satisfied,
            "completed_steps": self.completed_steps,
            "failed_steps": self.failed_steps,
            "skipped_steps": self.skipped_steps,
            "reason": self.reason,
            "step_evidence": [item.to_dict() for item in self.step_evidence],
        }
