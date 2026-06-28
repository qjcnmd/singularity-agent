from __future__ import annotations

import re
from dataclasses import dataclass, field
from typing import Any


class TaskContractSchemaError(ValueError):
    pass


@dataclass(frozen=True)
class AcceptanceCriterion:
    criterion_id: str
    description: str
    evidence: list[str]
    required: bool = True

    def to_dict(self) -> dict[str, Any]:
        return {
            "criterion_id": self.criterion_id,
            "description": self.description,
            "evidence": self.evidence,
            "required": self.required,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> AcceptanceCriterion:
        return cls(
            criterion_id=str(payload["criterion_id"]),
            description=str(payload.get("description") or payload["criterion_id"]),
            evidence=[str(item) for item in payload.get("evidence") or []],
            required=bool(payload.get("required", True)),
        )


@dataclass(frozen=True)
class Deliverable:
    kind: str
    description: str
    path: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return {"kind": self.kind, "description": self.description, "path": self.path}

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> Deliverable:
        return cls(
            kind=str(payload.get("kind") or "artifact"),
            description=str(payload.get("description") or ""),
            path=payload.get("path"),
        )


@dataclass(frozen=True)
class Constraint:
    description: str
    source: str = "user"

    def to_dict(self) -> dict[str, Any]:
        return {"description": self.description, "source": self.source}

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> Constraint:
        return cls(
            description=str(payload.get("description") or ""),
            source=str(payload.get("source") or "user"),
        )


@dataclass(frozen=True)
class VerificationRequirement:
    description: str
    command: list[str] | None = None
    required: bool = True

    def to_dict(self) -> dict[str, Any]:
        return {
            "description": self.description,
            "command": self.command,
            "required": self.required,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> VerificationRequirement:
        command = payload.get("command")
        return cls(
            description=str(payload.get("description") or ""),
            command=[str(item) for item in command] if isinstance(command, list) else None,
            required=bool(payload.get("required", True)),
        )


@dataclass(frozen=True)
class ReportRequirement:
    description: str
    sections: list[str] = field(default_factory=list)
    required: bool = True

    def to_dict(self) -> dict[str, Any]:
        return {
            "description": self.description,
            "sections": self.sections,
            "required": self.required,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> ReportRequirement:
        return cls(
            description=str(payload.get("description") or ""),
            sections=[str(item) for item in payload.get("sections") or []],
            required=bool(payload.get("required", True)),
        )


@dataclass(frozen=True)
class EvidenceRequirement:
    evidence_key: str
    description: str
    required: bool = True

    def to_dict(self) -> dict[str, Any]:
        return {
            "evidence_key": self.evidence_key,
            "description": self.description,
            "required": self.required,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> EvidenceRequirement:
        return cls(
            evidence_key=str(payload["evidence_key"]),
            description=str(payload.get("description") or payload["evidence_key"]),
            required=bool(payload.get("required", True)),
        )


@dataclass(frozen=True)
class TaskContract:
    user_goal: str
    acceptance_criteria: list[AcceptanceCriterion]
    deliverables: list[Deliverable] = field(default_factory=list)
    constraints: list[Constraint] = field(default_factory=list)
    verification_requirements: list[VerificationRequirement] = field(default_factory=list)
    report_requirements: list[ReportRequirement] = field(default_factory=list)
    evidence_requirements: list[EvidenceRequirement] = field(default_factory=list)
    source: str = "rules"
    version: int = 1

    def smoke_commands(self) -> list[list[str]]:
        return [
            list(requirement.command)
            for requirement in self.verification_requirements
            if requirement.required and requirement.command
        ]

    def to_dict(self) -> dict[str, Any]:
        return {
            "version": self.version,
            "source": self.source,
            "user_goal": self.user_goal,
            "acceptance_criteria": [item.to_dict() for item in self.acceptance_criteria],
            "deliverables": [item.to_dict() for item in self.deliverables],
            "constraints": [item.to_dict() for item in self.constraints],
            "verification_requirements": [
                item.to_dict() for item in self.verification_requirements
            ],
            "report_requirements": [item.to_dict() for item in self.report_requirements],
            "evidence_requirements": [item.to_dict() for item in self.evidence_requirements],
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> TaskContract:
        return cls(
            version=int(payload.get("version") or 1),
            source=str(payload.get("source") or "structured_output"),
            user_goal=str(payload.get("user_goal") or ""),
            acceptance_criteria=[
                AcceptanceCriterion.from_dict(item)
                for item in payload.get("acceptance_criteria") or []
            ],
            deliverables=[Deliverable.from_dict(item) for item in payload.get("deliverables") or []],
            constraints=[Constraint.from_dict(item) for item in payload.get("constraints") or []],
            verification_requirements=[
                VerificationRequirement.from_dict(item)
                for item in payload.get("verification_requirements") or []
            ],
            report_requirements=[
                ReportRequirement.from_dict(item)
                for item in payload.get("report_requirements") or []
            ],
            evidence_requirements=[
                EvidenceRequirement.from_dict(item)
                for item in payload.get("evidence_requirements") or []
            ],
        )

    @classmethod
    def validate_payload(cls, payload: dict[str, Any]) -> TaskContract:
        if not isinstance(payload, dict):
            raise TaskContractSchemaError("TaskContract payload must be an object.")
        try:
            contract = cls.from_dict(payload)
        except Exception as exc:
            raise TaskContractSchemaError(f"Invalid TaskContract payload: {exc}") from exc
        for criterion in contract.acceptance_criteria:
            if not criterion.criterion_id:
                raise TaskContractSchemaError("AcceptanceCriterion.criterion_id is required.")
            if criterion.required and not criterion.evidence:
                raise TaskContractSchemaError(
                    f"AcceptanceCriterion {criterion.criterion_id} requires evidence."
                )
        for requirement in contract.verification_requirements:
            if requirement.command is not None and not requirement.command:
                raise TaskContractSchemaError("VerificationRequirement.command cannot be empty.")
        return contract


class TaskContractBuilder:
    _PATH_RE = re.compile(r"(?P<path>[\w./\\-]+\.(?:py|md|txt|json|toml|ya?ml))")

    def build(self, user_goal: str, *, structured_output: dict[str, Any] | None = None) -> TaskContract:
        if structured_output:
            return self.from_structured_output(structured_output, fallback_goal=user_goal)
        return self.from_rules(user_goal)

    def from_structured_output(
        self,
        payload: dict[str, Any],
        *,
        fallback_goal: str = "",
    ) -> TaskContract:
        merged = {"user_goal": fallback_goal, **payload, "source": payload.get("source") or "model"}
        try:
            contract = TaskContract.validate_payload(merged)
        except TaskContractSchemaError:
            return self.from_rules(fallback_goal)
        if not contract.acceptance_criteria:
            return self.from_rules(contract.user_goal or fallback_goal)
        return contract

    def from_rules(self, user_goal: str) -> TaskContract:
        normalized = " ".join(user_goal.split())
        lowered = normalized.lower()
        paths = [match.group("path").replace("\\", "/") for match in self._PATH_RE.finditer(normalized)]
        deliverables: list[Deliverable] = []
        criteria: list[AcceptanceCriterion] = []
        verifications: list[VerificationRequirement] = []
        evidence: list[EvidenceRequirement] = [
            EvidenceRequirement("inspected_files", "Relevant files or task context were inspected.")
        ]

        if paths and any(word in lowered for word in ["create", "write", "add", "生成", "创建", "新增"]):
            path = paths[0]
            deliverables.append(Deliverable("file", f"Create or update {path}.", path=path))
            criteria.append(
                AcceptanceCriterion(
                    _criterion_id("deliver", path),
                    f"{path} is created or updated through mutation evidence.",
                    ["applied_changes"],
                )
            )
            evidence.append(EvidenceRequirement("applied_changes", "Workspace mutation evidence exists."))
            if path.endswith(".py") and any(word in lowered for word in ["smoke", "run", "verify", "验证", "运行"]):
                command = ["python", path]
                verifications.append(
                    VerificationRequirement(
                        f"Run smoke verification for {path}.",
                        command=command,
                    )
                )
                criteria.append(
                    AcceptanceCriterion(
                        _criterion_id("verify", path),
                        f"Smoke verification for {path} passes.",
                        ["verification_results"],
                    )
                )
                evidence.append(
                    EvidenceRequirement("verification_results", "Verification evidence is ready.")
                )

        if any(word in lowered for word in ["report", "报告", "实验报告"]):
            deliverables.append(Deliverable("report", "Produce the requested report."))
            report = ReportRequirement(
                "Final report must cover goal, requirements, changes, verification, and risks.",
                sections=["goal", "requirements", "changes", "verification", "risks"],
            )
            criteria.append(
                AcceptanceCriterion(
                    "report_obligation_recorded",
                    "Report obligation is recorded in the task contract.",
                    ["task_contract"],
                    required=False,
                )
            )
            return TaskContract(
                user_goal=normalized,
                acceptance_criteria=criteria or [
                    AcceptanceCriterion("report_requested", "Report request was captured.", ["task_contract"])
                ],
                deliverables=deliverables,
                constraints=_constraints(normalized),
                verification_requirements=verifications,
                report_requirements=[report],
                evidence_requirements=evidence,
            )

        return TaskContract(
            user_goal=normalized,
            acceptance_criteria=criteria,
            deliverables=deliverables,
            constraints=_constraints(normalized),
            verification_requirements=verifications,
            report_requirements=[],
            evidence_requirements=evidence,
        )


def _criterion_id(prefix: str, path: str) -> str:
    safe = re.sub(r"[^a-zA-Z0-9]+", "_", path).strip("_").lower()
    return f"{prefix}_{safe}"


def _constraints(goal: str) -> list[Constraint]:
    constraints: list[Constraint] = []
    for marker in ["do not", "don't", "不要", "不得", "禁止"]:
        if marker in goal.lower():
            constraints.append(Constraint(goal, source="user"))
            break
    return constraints
