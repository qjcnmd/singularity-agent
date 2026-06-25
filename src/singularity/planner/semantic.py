from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any
from uuid import uuid4

from singularity.planner.contract import AcceptanceCriterion, TaskContract


@dataclass(frozen=True)
class PlanDependency:
    step_id: str
    reason: str = "requires_previous_step"

    def to_dict(self) -> dict[str, Any]:
        return {"step_id": self.step_id, "reason": self.reason}

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "PlanDependency":
        return cls(step_id=str(payload["step_id"]), reason=str(payload.get("reason") or "requires_previous_step"))


@dataclass(frozen=True)
class ExpectedEvidence:
    evidence_key: str
    acceptance_criterion_id: str | None = None
    description: str = ""

    def to_dict(self) -> dict[str, Any]:
        return {
            "evidence_key": self.evidence_key,
            "acceptance_criterion_id": self.acceptance_criterion_id,
            "description": self.description,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "ExpectedEvidence":
        return cls(
            evidence_key=str(payload["evidence_key"]),
            acceptance_criterion_id=payload.get("acceptance_criterion_id"),
            description=str(payload.get("description") or ""),
        )


@dataclass(frozen=True)
class FallbackStep:
    reason: str
    next_action: str
    allowed_capabilities: list[str] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {
            "reason": self.reason,
            "next_action": self.next_action,
            "allowed_capabilities": self.allowed_capabilities,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "FallbackStep":
        return cls(
            reason=str(payload["reason"]),
            next_action=str(payload.get("next_action") or "replan"),
            allowed_capabilities=[str(item) for item in payload.get("allowed_capabilities") or []],
        )


@dataclass(frozen=True)
class PlanStep:
    step_id: str
    title: str
    kind: str
    acceptance_criterion_id: str | None = None
    dependencies: list[PlanDependency] = field(default_factory=list)
    allowed_capabilities: list[str] = field(default_factory=list)
    expected_evidence: list[ExpectedEvidence] = field(default_factory=list)
    fallback_steps: list[FallbackStep] = field(default_factory=list)
    status: str = "pending"

    def to_dict(self) -> dict[str, Any]:
        return {
            "step_id": self.step_id,
            "title": self.title,
            "kind": self.kind,
            "acceptance_criterion_id": self.acceptance_criterion_id,
            "dependencies": [item.to_dict() for item in self.dependencies],
            "allowed_capabilities": self.allowed_capabilities,
            "expected_evidence": [item.to_dict() for item in self.expected_evidence],
            "fallback_steps": [item.to_dict() for item in self.fallback_steps],
            "status": self.status,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "PlanStep":
        return cls(
            step_id=str(payload["step_id"]),
            title=str(payload.get("title") or payload["step_id"]),
            kind=str(payload.get("kind") or "task"),
            acceptance_criterion_id=payload.get("acceptance_criterion_id"),
            dependencies=[PlanDependency.from_dict(item) for item in payload.get("dependencies") or []],
            allowed_capabilities=[str(item) for item in payload.get("allowed_capabilities") or []],
            expected_evidence=[ExpectedEvidence.from_dict(item) for item in payload.get("expected_evidence") or []],
            fallback_steps=[FallbackStep.from_dict(item) for item in payload.get("fallback_steps") or []],
            status=str(payload.get("status") or "pending"),
        )


@dataclass(frozen=True)
class RollingPlan:
    plan_id: str
    user_goal: str
    steps: list[PlanStep]
    current_step_id: str
    version: int = 1

    def current_step(self) -> PlanStep | None:
        for step in self.steps:
            if step.step_id == self.current_step_id:
                return step
        return self.steps[0] if self.steps else None

    def to_dict(self) -> dict[str, Any]:
        return {
            "plan_id": self.plan_id,
            "version": self.version,
            "user_goal": self.user_goal,
            "current_step_id": self.current_step_id,
            "steps": [step.to_dict() for step in self.steps],
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "RollingPlan":
        steps = [PlanStep.from_dict(item) for item in payload.get("steps") or []]
        return cls(
            plan_id=str(payload.get("plan_id") or f"rolling_{uuid4().hex[:12]}"),
            version=int(payload.get("version") or 1),
            user_goal=str(payload.get("user_goal") or ""),
            current_step_id=str(payload.get("current_step_id") or (steps[0].step_id if steps else "")),
            steps=steps,
        )


class SemanticPlanner:
    def initial_plan(self, task_contract: TaskContract | dict[str, Any]) -> RollingPlan:
        contract = _contract(task_contract)
        steps = [
            PlanStep(
                step_id="step_inspect_context",
                title="Inspect relevant context",
                kind="inspect",
                allowed_capabilities=["list_files", "read_file", "search_text", "workspace_health"],
                expected_evidence=[ExpectedEvidence("inspected_files", description="Relevant context inspected.")],
                fallback_steps=[FallbackStep("missing_context", "read_relevant_files", ["read_file", "search_text"])],
            )
        ]
        previous_step_id = steps[0].step_id
        for criterion in contract.acceptance_criteria:
            step = self._step_for_criterion(criterion, previous_step_id=previous_step_id)
            steps.append(step)
            previous_step_id = step.step_id
        return RollingPlan(
            plan_id=f"rolling_{uuid4().hex[:12]}",
            user_goal=contract.user_goal,
            steps=steps,
            current_step_id=steps[0].step_id,
        )

    def repair_plan(
        self,
        failure_analysis: Any,
        *,
        task_contract: TaskContract | dict[str, Any],
    ) -> RollingPlan:
        contract = _contract(task_contract)
        analysis = failure_analysis.to_dict() if hasattr(failure_analysis, "to_dict") else dict(failure_analysis)
        criterion = _failed_criterion(contract, analysis)
        target = _first_suspect_file(analysis)
        failure_category = str(analysis.get("failure_category") or analysis.get("failure_type") or "failed verification")
        allowed_capabilities = _repair_capabilities(analysis)
        verification_plan = [str(item) for item in analysis.get("verification_plan") or [] if item]
        expected = [
            ExpectedEvidence(
                evidence_key=item,
                acceptance_criterion_id=criterion.criterion_id if criterion else None,
                description="Repair must be verified against the failed criterion.",
            )
            for item in ((criterion.evidence if criterion else ["verification_results"]) or ["verification_results"])
        ]
        step = PlanStep(
            step_id=f"repair_{analysis.get('check_id') or uuid4().hex[:8]}",
            title=f"Repair {failure_category}: {target or 'failed verification'}",
            kind="repair",
            acceptance_criterion_id=criterion.criterion_id if criterion else None,
            allowed_capabilities=allowed_capabilities,
            expected_evidence=expected,
            fallback_steps=[
                FallbackStep(
                    "repair_step_failed",
                    "replan_or_ask_user",
                    sorted({"read_file", "search_text", "run_verification", *allowed_capabilities}),
                )
            ],
        )
        if verification_plan:
            step.expected_evidence.append(
                ExpectedEvidence(
                    evidence_key="repair_contract_verification_plan",
                    acceptance_criterion_id=criterion.criterion_id if criterion else None,
                    description="Repair must execute: " + "; ".join(verification_plan[:3]),
                )
            )
        return RollingPlan(
            plan_id=f"rolling_repair_{uuid4().hex[:12]}",
            user_goal=contract.user_goal,
            steps=[step],
            current_step_id=step.step_id,
        )

    def _step_for_criterion(self, criterion: AcceptanceCriterion, *, previous_step_id: str) -> PlanStep:
        return PlanStep(
            step_id=f"step_{criterion.criterion_id}",
            title=criterion.description,
            kind=_kind_for_evidence(criterion.evidence),
            acceptance_criterion_id=criterion.criterion_id,
            dependencies=[PlanDependency(previous_step_id)],
            allowed_capabilities=_capabilities_for_evidence(criterion.evidence),
            expected_evidence=[
                ExpectedEvidence(
                    evidence_key=item,
                    acceptance_criterion_id=criterion.criterion_id,
                    description=f"Evidence required for {criterion.criterion_id}.",
                )
                for item in criterion.evidence
            ],
            fallback_steps=[FallbackStep("missing_evidence", "replan", ["read_file", "search_text"])],
        )


def _contract(value: TaskContract | dict[str, Any]) -> TaskContract:
    return value if isinstance(value, TaskContract) else TaskContract.from_dict(value)


def _kind_for_evidence(evidence: list[str]) -> str:
    if "verification_results" in evidence:
        return "verify"
    if "applied_changes" in evidence:
        return "change"
    if "final_report_ready" in evidence:
        return "report"
    return "inspect"


def _capabilities_for_evidence(evidence: list[str]) -> list[str]:
    capabilities: set[str] = {"read_file"}
    if "applied_changes" in evidence:
        capabilities.update({"apply_patch", "inspect_diff", "write_file"})
    if "verification_results" in evidence:
        capabilities.update({"get_verification_result", "inspect_diff", "run_verification"})
    if "final_report_ready" in evidence:
        capabilities.update({"get_verification_result", "inspect_diff", "workspace_health"})
    if "task_contract" in evidence:
        capabilities.update({"workspace_health"})
    return sorted(capabilities)


def _failed_criterion(contract: TaskContract, analysis: dict[str, Any]) -> AcceptanceCriterion | None:
    suspect = set(str(item) for item in analysis.get("suspect_files") or [])
    verification = [criterion for criterion in contract.acceptance_criteria if "verification_results" in criterion.evidence]
    if verification:
        return verification[0]
    for criterion in contract.acceptance_criteria:
        if any(path in criterion.description for path in suspect):
            return criterion
    return contract.acceptance_criteria[0] if contract.acceptance_criteria else None


def _first_suspect_file(analysis: dict[str, Any]) -> str | None:
    suspects = (
        analysis.get("target_files")
        or analysis.get("affected_files")
        or analysis.get("suspect_files")
        or []
    )
    return str(suspects[0]) if suspects else None


def _repair_capabilities(analysis: dict[str, Any]) -> list[str]:
    capabilities = {str(item) for item in analysis.get("allowed_tool_names") or [] if item}
    for candidate in analysis.get("action_candidates") or []:
        if isinstance(candidate, dict):
            capabilities.update(str(item) for item in candidate.get("tool_hints") or [] if item)
    if analysis.get("verification_plan"):
        capabilities.update({"get_verification_result", "run_verification"})
    if not capabilities:
        capabilities.update({"apply_patch", "inspect_diff", "read_file", "run_verification"})
    return sorted(capabilities)
