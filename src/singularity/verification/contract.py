from __future__ import annotations

import shlex
from dataclasses import dataclass, field
from typing import Any
from uuid import uuid4

INTERNAL_VERIFICATION_REFS = {"final_review"}


@dataclass(frozen=True)
class VerificationStep:
    """A single executable verification step within a verification contract."""

    step_id: str
    command: str
    kind: str = "smoke"
    required: bool = True

    @property
    def command_argv(self) -> list[str]:
        """Normalized argv for command matching."""

        text = self.command.strip()
        if not text:
            return []
        try:
            return shlex.split(text)
        except ValueError:
            return text.split()

    def matches_command(self, argv: list[str] | None) -> bool:
        """Check whether an argv matches this step's command (order-insensitive args for tail)."""
        if not argv:
            return False
        step_argv = self.command_argv
        if not step_argv:
            return False
        # Prefix match: the executing command must start with the step's command
        if len(argv) < len(step_argv):
            return False
        return argv[: len(step_argv)] == step_argv

    def to_dict(self) -> dict[str, Any]:
        return {
            "step_id": self.step_id,
            "command": self.command,
            "command_argv": self.command_argv,
            "kind": self.kind,
            "required": self.required,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> VerificationStep:
        return cls(
            step_id=str(payload.get("step_id") or ""),
            command=str(payload.get("command") or ""),
            kind=str(payload.get("kind") or "smoke"),
            required=bool(payload.get("required", True)),
        )


@dataclass(frozen=True)
class VerificationContract:
    """Structured verification requirements derived from a repair contract.

    Replaces loose ``verification_plan: list[str]`` with typed steps, status
    tracking, and satisfaction evidence.
    """

    contract_id: str
    steps: list[VerificationStep]
    status: str = "pending"
    validation_errors: list[str] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {
            "contract_id": self.contract_id,
            "steps": [step.to_dict() for step in self.steps],
            "status": self.status,
            "validation_errors": list(self.validation_errors),
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> VerificationContract:
        steps = [VerificationStep.from_dict(item) for item in (payload.get("steps") or [])]
        return cls(
            contract_id=str(payload.get("contract_id") or ""),
            steps=steps,
            status=str(payload.get("status") or "pending"),
            validation_errors=list(payload.get("validation_errors") or []),
        )

    @classmethod
    def from_plan_strings(
        cls, plan: list[str], *, contract_id: str | None = None
    ) -> VerificationContract:
        steps: list[VerificationStep] = []
        for index, text in enumerate(plan):
            text = text.strip()
            if not text or text in INTERNAL_VERIFICATION_REFS:
                continue
            steps.append(
                VerificationStep(
                    step_id=f"vstep_{index}",
                    command=text,
                    kind="smoke",
                    required=True,
                )
            )
        return cls(
            contract_id=contract_id or f"vcontract_{uuid4().hex[:12]}",
            steps=steps,
        )

    @classmethod
    def empty(cls) -> VerificationContract:
        return cls(contract_id=f"vcontract_{uuid4().hex[:12]}", steps=[])

    @property
    def is_valid(self) -> bool:
        return bool(self.steps) and not self.validation_errors

    @property
    def allowed_commands(self) -> list[list[str]]:
        """All allowed command argvs from contract steps."""
        return [step.command_argv for step in self.steps if step.command_argv]

    def is_command_allowed(self, argv: list[str] | None) -> bool:
        """Check whether a command argv matches any step in this contract."""
        if not argv:
            return False
        if not self.steps:
            return True  # empty contract = no constraint
        return any(step.matches_command(argv) for step in self.steps)

    def step_for_command(self, argv: list[str] | None) -> VerificationStep | None:
        """Find the contract step that matches the given command argv."""
        if not argv:
            return None
        for step in self.steps:
            if step.matches_command(argv):
                return step
        return None
