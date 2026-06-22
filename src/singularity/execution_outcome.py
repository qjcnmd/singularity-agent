from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from typing import Any


class ExecutionOutcomeStatus(str, Enum):
    SUCCESS = "success"
    RETRYABLE = "retryable"
    REPLAN_REQUIRED = "replan_required"
    APPROVAL_REQUIRED = "approval_required"
    USER_INPUT_REQUIRED = "user_input_required"
    BLOCKED = "blocked"
    FATAL = "fatal"


@dataclass(frozen=True)
class ExecutionOutcome:
    status: ExecutionOutcomeStatus
    source: str
    reason: str
    error_code: str | None = None
    missing_evidence: list[str] = field(default_factory=list)
    next_action: str | None = None
    observation_summary: str = ""
    retry_allowed: bool = True
    metadata: dict[str, Any] = field(default_factory=dict)

    def __post_init__(self) -> None:
        object.__setattr__(self, "status", ExecutionOutcomeStatus(self.status))
        object.__setattr__(self, "missing_evidence", list(self.missing_evidence))
        object.__setattr__(self, "metadata", dict(self.metadata))

    def to_dict(self) -> dict[str, Any]:
        return {
            "status": self.status.value,
            "source": self.source,
            "reason": self.reason,
            "error_code": self.error_code,
            "missing_evidence": self.missing_evidence,
            "next_action": self.next_action,
            "observation_summary": self.observation_summary,
            "retry_allowed": self.retry_allowed,
            "metadata": self.metadata,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "ExecutionOutcome":
        return cls(
            status=ExecutionOutcomeStatus(payload["status"]),
            source=str(payload.get("source") or "unknown"),
            reason=str(payload.get("reason") or ""),
            error_code=payload.get("error_code"),
            missing_evidence=list(payload.get("missing_evidence") or []),
            next_action=payload.get("next_action"),
            observation_summary=str(payload.get("observation_summary") or ""),
            retry_allowed=bool(payload.get("retry_allowed", True)),
            metadata=dict(payload.get("metadata") or {}),
        )
