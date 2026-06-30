from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

from singularity.command import CommandPolicy
from singularity.verification.models import (
    VerificationCheck,
    VerificationDecision,
)


@dataclass(frozen=True)
class VerificationPolicyResult:
    decision: VerificationDecision
    reasons: list[str]
    risk_tags: list[str]
    error_code: str | None = None
    command_policy: dict | None = None

    def to_dict(self) -> dict:
        return {
            "decision": self.decision.value,
            "reasons": self.reasons,
            "risk_tags": self.risk_tags,
            "error_code": self.error_code,
            "command_policy": self.command_policy,
        }


class VerificationPolicy:
    def __init__(self, command_policy: CommandPolicy | None = None) -> None:
        self.command_policy = command_policy or CommandPolicy()

    def evaluate(
        self,
        check: VerificationCheck,
        *,
        workspace_root: Path,
    ) -> VerificationPolicyResult:
        if check.command is None:
            decision = (
                VerificationDecision.BLOCKED
                if check.required
                else VerificationDecision.ALLOW
            )
            return VerificationPolicyResult(
                decision=decision,
                reasons=[check.skip_reason or "Check has no executable command."],
                risk_tags=check.risk_tags,
                error_code="check_blocked" if check.required else None,
            )

        _ = workspace_root
        risk_tags = sorted(
            {
                *check.risk_tags,
                *(tag.value for tag in self.command_policy.classify(check.command)),
            }
        )
        return VerificationPolicyResult(
            decision=VerificationDecision.ALLOW,
            reasons=["Verification command policy is enforced by PolicyEngine at execution time."],
            risk_tags=risk_tags,
        )
