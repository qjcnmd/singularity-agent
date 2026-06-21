from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

from singularity.command import (
    CommandDecision,
    CommandPolicy,
    CommandRequest,
    NetworkMode,
)
from singularity.verification.models import (
    CheckKind,
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

        high_risk = self._high_risk_reason(check, check.command)
        if high_risk is not None:
            return VerificationPolicyResult(
                decision=VerificationDecision.REQUIRE_REVIEW,
                reasons=[high_risk],
                risk_tags=sorted({*check.risk_tags, "high_risk_verification"}),
                error_code="check_review_required",
            )

        command_result = self.command_policy.evaluate(
            check.command,
            workspace_root=workspace_root,
        )
        risk_tags = [tag.value for tag in command_result.risk_tags]
        if command_result.decision == CommandDecision.ALLOW:
            return VerificationPolicyResult(
                decision=VerificationDecision.ALLOW,
                reasons=command_result.reasons,
                risk_tags=risk_tags,
                command_policy=command_result.to_dict(),
            )
        if command_result.decision == CommandDecision.REQUIRE_REVIEW:
            return VerificationPolicyResult(
                decision=VerificationDecision.REQUIRE_REVIEW,
                reasons=command_result.reasons,
                risk_tags=risk_tags,
                error_code="check_review_required",
                command_policy=command_result.to_dict(),
            )
        return VerificationPolicyResult(
            decision=VerificationDecision.DENY,
            reasons=command_result.reasons,
            risk_tags=risk_tags,
            error_code="check_policy_denied",
            command_policy=command_result.to_dict(),
        )

    @staticmethod
    def _high_risk_reason(
        check: VerificationCheck,
        request: CommandRequest,
    ) -> str | None:
        argv = [part.lower() for part in (request.argv or [])]
        joined = " ".join(argv)
        if check.kind == CheckKind.INTEGRATION_TEST and request.network_mode != NetworkMode.DISABLED:
            return "Integration verification with network access requires review."
        if any(part in {"docker", "docker-compose", "podman"} for part in argv[:1]):
            return "Container-based verification requires review."
        if any(token in joined for token in (" migrate", " db:migrate", " prisma migrate", "alembic upgrade")):
            return "Database migration commands require review."
        if any(token in joined for token in ("npm install", "pnpm install", "yarn install", "pip install", "uv sync")):
            return "Package manager install/sync commands require review."
        if request.shell is not None:
            return "Shell verification commands require review."
        return None
