from __future__ import annotations

import json
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any
from uuid import uuid4

from singularity.execution_outcome import ExecutionOutcome, ExecutionOutcomeStatus
from singularity.model import (
    ContentBlock,
    ModelBudget,
    ModelMessage,
    ModelPreferences,
    ModelPurpose,
    ModelRole,
    ModelTurnRequest,
    ModelTurnStatus,
    ToolChoiceMode,
    ToolChoicePolicy,
)
from singularity.observability.models import TraceEventType, TraceSeverity

SUMMARY_LIMIT = 700
TAIL_LIMIT = 400
MIN_REPAIR_CONFIDENCE = 0.45
FAILURE_CATEGORY_PATTERN = re.compile(r"^[a-z][a-z0-9_]{2,80}$")
BLOCKED_FAILURE_CATEGORIES = {
    "approval_required",
    "permission_denied",
    "policy_blocked",
    "policy_denied",
    "risk_escalated",
    "sandbox_required",
    "missing_information",
    "user_input_required",
    "failure_analysis_unavailable",
    "failure_analysis_invalid_json",
    "failure_analysis_schema_invalid",
    "low_confidence",
}
ACTION_TYPES = {"inspect", "analyze", "edit", "verify", "ask_user"}
TOOL_HINTS = {
    "apply_patch",
    "get_verification_result",
    "inspect_diff",
    "read_file",
    "run_verification",
    "search_text",
    "workspace_health",
    "write_file",
}
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
        import shlex

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
    def from_dict(cls, payload: dict[str, Any]) -> "VerificationStep":
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
    def from_dict(cls, payload: dict[str, Any]) -> "VerificationContract":
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
    ) -> "VerificationContract":
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
    def empty(cls) -> "VerificationContract":
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


@dataclass(frozen=True)
class FailureAnalysisRequest:
    request_id: str
    run_id: str
    session_id: str
    task_id: str
    phase_id: str
    workspace_root: str
    failure_source: str
    failure_summary: str
    failure_sources: list[dict[str, Any]]
    context_references: list[str] = field(default_factory=list)
    recent_tail: list[dict[str, Any]] = field(default_factory=list)
    verification_log_refs: list[str] = field(default_factory=list)
    changed_files: list[str] = field(default_factory=list)
    evidence_refs: list[str] = field(default_factory=list)
    metadata: dict[str, Any] = field(default_factory=dict)
    risk_points: list[dict[str, Any]] = field(default_factory=list)
    repair_policy: dict[str, Any] | None = None
    verification_strategies: list[dict[str, Any]] = field(default_factory=list)

    @classmethod
    def from_planner(
        cls,
        planner: Any,
        context: Any,
        *,
        failure_source: str,
        outcome: ExecutionOutcome | None = None,
        turn: int | None = None,
    ) -> "FailureAnalysisRequest":
        state = getattr(planner, "state", None)
        evidence = getattr(planner, "evidence", None)
        task_id = str(getattr(state, "task_id", "") or getattr(planner, "task_id", "") or "")
        session_id = str(getattr(state, "session_id", "") or getattr(planner, "session_id", "") or "")
        phase_id = str(getattr(state, "current_phase", "") or "failure_analysis")
        failure_sources = _failure_sources(evidence, outcome=outcome)
        summary = _failure_summary(failure_sources, outcome=outcome)
        risk_points = list(getattr(state, "risk_points", None) or [])
        repair_policy = getattr(state, "repair_policy", None)
        verification_strategies = list(getattr(state, "verification_strategies", None) or [])
        return cls(
            request_id=f"failure_analysis_{uuid4().hex[:12]}",
            run_id=str(getattr(context, "run_id", "") or session_id or task_id),
            session_id=session_id,
            task_id=task_id,
            phase_id=phase_id,
            workspace_root=str(getattr(planner, "workspace_root", "") or ""),
            failure_source=failure_source,
            failure_summary=summary,
            failure_sources=failure_sources,
            context_references=_context_references(context, evidence),
            recent_tail=_recent_tail(context),
            verification_log_refs=_verification_log_refs(evidence),
            changed_files=_changed_files(evidence),
            evidence_refs=_evidence_refs(evidence),
            metadata={"turn": turn} if turn is not None else {},
            risk_points=risk_points,
            repair_policy=repair_policy if isinstance(repair_policy, dict) else None,
            verification_strategies=verification_strategies,
        )

    @property
    def has_failure(self) -> bool:
        return bool(self.failure_sources or self.failure_summary)

    @property
    def fingerprint(self) -> str:
        payload: dict[str, Any] = {
            "source": self.failure_source,
            "summary": self.failure_summary,
            "failures": _fingerprint_sources(self.failure_sources),
        }
        return json.dumps(payload, ensure_ascii=False, sort_keys=True, default=str)

    @property
    def failure_evidence_refs(self) -> list[str]:
        refs: list[str] = []
        for source in self.failure_sources:
            _append_unique(refs, source.get("outcome_ref"))
            _append_unique(refs, source.get("tool_call_id"))
            _append_unique(refs, source.get("command_id"))
            _append_unique(refs, source.get("check_id"))
            evidence = source.get("evidence")
            if isinstance(evidence, dict):
                _append_unique(refs, evidence.get("command_id"))
                _append_unique(refs, evidence.get("artifact_ref"))
                _append_unique(refs, evidence.get("artifact_path"))
            assessment = source.get("completion_assessment")
            if isinstance(assessment, dict):
                for check_id in assessment.get("failed_checks") or []:
                    _append_unique(refs, check_id)
        return refs or self.evidence_refs

    @property
    def allowed_target_files(self) -> list[str]:
        files: list[str] = []
        for path in self.changed_files:
            _append_unique(files, _normalize_workspace_path(path, workspace_root=self.workspace_root))
        for source in self.failure_sources:
            for path in _paths_from_failure_source(source):
                _append_unique(files, _normalize_workspace_path(path, workspace_root=self.workspace_root))
        return files

    def to_model_payload(self) -> dict[str, Any]:
        return {
            "request_id": self.request_id,
            "failure_source": self.failure_source,
            "failure_summary": self.failure_summary,
            "failure_sources": self.failure_sources[-8:],
            "context_references": self.context_references[-20:],
            "recent_tail": self.recent_tail[-8:],
            "verification_log_refs": self.verification_log_refs[-10:],
            "changed_files": self.changed_files[-30:],
            "allowed_target_files": self.allowed_target_files[-30:],
            "evidence_refs": self.evidence_refs[-30:],
            "risk_points": self.risk_points[-10:],
            "repair_policy": self.repair_policy,
            "verification_strategies": self.verification_strategies[-10:],
        }

    def to_dict(self) -> dict[str, Any]:
        return {
            "request_id": self.request_id,
            "run_id": self.run_id,
            "session_id": self.session_id,
            "task_id": self.task_id,
            "phase_id": self.phase_id,
            "workspace_root": self.workspace_root,
            "failure_source": self.failure_source,
            "failure_summary": self.failure_summary,
            "failure_sources": self.failure_sources,
            "context_references": self.context_references,
            "recent_tail": self.recent_tail,
            "verification_log_refs": self.verification_log_refs,
            "changed_files": self.changed_files,
            "evidence_refs": self.evidence_refs,
            "metadata": self.metadata,
            "risk_points": self.risk_points,
            "repair_policy": self.repair_policy,
            "verification_strategies": self.verification_strategies,
        }


@dataclass(frozen=True)
class FailureAnalysisResult:
    analysis_id: str
    request_id: str
    root_cause: str
    failure_category: str
    affected_files: list[str]
    evidence_refs: list[str]
    repair_strategy: str
    next_actions: list[str]
    verification_plan: list[str]
    confidence: float
    needs_user_input: bool
    blocked_reason: str | None = None
    raw_response_ref: str | None = None
    verification_contract: VerificationContract = field(
        default_factory=VerificationContract.empty
    )

    @classmethod
    def from_model_payload(
        cls,
        payload: dict[str, Any],
        *,
        request: FailureAnalysisRequest,
        raw_response_ref: str | None = None,
    ) -> "FailureAnalysisResult":
        needs_user_input = _bool_required(payload, "needs_user_input")
        confidence = _confidence_required(payload.get("confidence"))
        category = _required_text(payload, "failure_category")
        category = category.replace("/", "_").replace("-", "_")
        if not FAILURE_CATEGORY_PATTERN.match(category):
            raise ValueError(f"invalid failure_category: {category!r}")
        root_cause = _required_text(payload, "root_cause")
        evidence_refs = _strings_required(payload, "evidence_refs")
        _validate_evidence_refs(evidence_refs, request=request)
        affected = _validated_affected_files(payload.get("affected_files"), request=request)
        next_actions = _strings_required(payload, "next_actions")
        verification_plan = _strings_required(payload, "verification_plan") if not needs_user_input else _strings(
            payload.get("verification_plan")
        )
        verification_contract = VerificationContract.from_plan_strings(verification_plan)
        _validate_verification_plan(
            verification_plan,
            needs_user_input=needs_user_input,
            verification_contract=verification_contract,
        )
        blocked_reason = _text(payload.get("blocked_reason")) or None
        if needs_user_input and not blocked_reason:
            raise ValueError("blocked_reason is required when needs_user_input=true")
        if not needs_user_input and confidence < MIN_REPAIR_CONFIDENCE:
            raise ValueError(f"confidence below repair threshold: {confidence}")
        if not needs_user_input and not affected:
            raise ValueError("affected_files must identify at least one workspace target")
        return cls(
            analysis_id=str(payload.get("analysis_id") or f"failure_{uuid4().hex[:12]}"),
            request_id=request.request_id,
            root_cause=root_cause,
            failure_category=category,
            affected_files=affected[:20],
            evidence_refs=evidence_refs[:30],
            repair_strategy=_required_text(payload, "repair_strategy"),
            next_actions=next_actions[:12],
            verification_plan=verification_plan[:12],
            confidence=confidence,
            needs_user_input=needs_user_input,
            blocked_reason=blocked_reason,
            raw_response_ref=raw_response_ref,
            verification_contract=verification_contract,
        )

    @classmethod
    def blocked(
        cls,
        *,
        request: FailureAnalysisRequest,
        reason: str,
        category: str = "failure_analysis_unavailable",
    ) -> "FailureAnalysisResult":
        return cls(
            analysis_id=f"failure_{uuid4().hex[:12]}",
            request_id=request.request_id,
            root_cause=reason,
            failure_category=category,
            affected_files=list(request.allowed_target_files),
            evidence_refs=list(request.evidence_refs),
            repair_strategy="blocked",
            next_actions=[reason],
            verification_plan=[],
            confidence=0.0,
            needs_user_input=True,
            blocked_reason=reason,
        )

    def to_dict(self) -> dict[str, Any]:
        root = {
            "description": self.root_cause,
            "evidence": self.evidence_refs,
            "confidence": self.confidence,
        }
        return {
            "analysis_id": self.analysis_id,
            "request_id": self.request_id,
            "root_cause": root,
            "root_cause_text": self.root_cause,
            "failure_category": self.failure_category,
            "failure_type": self.failure_category,
            "affected_files": self.affected_files,
            "suspect_files": self.affected_files,
            "evidence_refs": self.evidence_refs,
            "repair_strategy": self.repair_strategy,
            "next_actions": self.next_actions,
            "verification_plan": self.verification_plan,
            "verification_contract": self.verification_contract.to_dict(),
            "confidence": self.confidence,
            "needs_user_input": self.needs_user_input,
            "blocked_reason": self.blocked_reason,
            "raw_response_ref": self.raw_response_ref,
        }


@dataclass(frozen=True)
class RepairActionCandidate:
    candidate_id: str
    action_type: str
    target_file: str | None
    rationale: str
    tool_hints: list[str]
    verification_ref: str | None = None
    confidence: float = 0.5

    def to_dict(self) -> dict[str, Any]:
        return {
            "candidate_id": self.candidate_id,
            "action_type": self.action_type,
            "target_file": self.target_file,
            "rationale": self.rationale,
            "tool_hints": self.tool_hints,
            "verification_ref": self.verification_ref,
            "confidence": self.confidence,
        }


@dataclass(frozen=True)
class RepairContract:
    contract_id: str
    analysis_id: str
    failure_category: str
    target_files: list[str]
    evidence_refs: list[str]
    action_candidates: list[RepairActionCandidate]
    verification_plan: list[str]
    confidence: float
    allowed_tool_names: list[str]
    needs_user_input: bool = False
    blocked_reason: str | None = None
    validation_errors: list[str] = field(default_factory=list)
    verification_contract: VerificationContract = field(
        default_factory=VerificationContract.empty
    )

    @classmethod
    def from_analysis(
        cls,
        analysis: FailureAnalysisResult,
        *,
        action_candidates: list[RepairActionCandidate],
    ) -> "RepairContract":
        allowed = _allowed_tools_from_candidates(action_candidates)
        if analysis.verification_plan or analysis.verification_contract.steps:
            allowed.extend(["run_verification", "get_verification_result"])
        errors = _repair_contract_errors(
            analysis=analysis,
            action_candidates=action_candidates,
            allowed_tool_names=allowed,
            verification_contract=analysis.verification_contract,
        )
        return cls(
            contract_id=f"repair_contract_{uuid4().hex[:12]}",
            analysis_id=analysis.analysis_id,
            failure_category=analysis.failure_category,
            target_files=list(analysis.affected_files),
            evidence_refs=list(analysis.evidence_refs),
            action_candidates=list(action_candidates),
            verification_plan=list(analysis.verification_plan),
            confidence=analysis.confidence,
            allowed_tool_names=sorted(dict.fromkeys(allowed)),
            needs_user_input=analysis.needs_user_input or bool(errors) or bool(analysis.blocked_reason),
            blocked_reason=analysis.blocked_reason or ("; ".join(errors) if errors else None),
            validation_errors=errors,
            verification_contract=analysis.verification_contract,
        )

    @classmethod
    def blocked(cls, analysis: FailureAnalysisResult, *, reason: str) -> "RepairContract":
        return cls(
            contract_id=f"repair_contract_{uuid4().hex[:12]}",
            analysis_id=analysis.analysis_id,
            failure_category=analysis.failure_category,
            target_files=list(analysis.affected_files),
            evidence_refs=list(analysis.evidence_refs),
            action_candidates=[],
            verification_plan=list(analysis.verification_plan),
            confidence=analysis.confidence,
            allowed_tool_names=[],
            needs_user_input=True,
            blocked_reason=reason,
            validation_errors=[reason],
            verification_contract=analysis.verification_contract,
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "contract_id": self.contract_id,
            "analysis_id": self.analysis_id,
            "failure_category": self.failure_category,
            "target_files": self.target_files,
            "evidence_refs": self.evidence_refs,
            "action_candidates": [item.to_dict() for item in self.action_candidates],
            "verification_plan": self.verification_plan,
            "verification_contract": self.verification_contract.to_dict(),
            "confidence": self.confidence,
            "allowed_tool_names": self.allowed_tool_names,
            "needs_user_input": self.needs_user_input,
            "blocked_reason": self.blocked_reason,
            "validation_errors": self.validation_errors,
        }


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
            "steps": [item.to_dict() for item in self.action_candidates],
            "next_actions": self.next_actions,
            "next_verification": {"commands": self.verification_plan},
            "verification_plan": self.verification_plan,
            "verification_contract": self.verification_contract.to_dict(),
            "evidence_refs": self.evidence_refs,
            "confidence": self.confidence,
            "needs_user_input": self.needs_user_input,
            "blocked_reason": self.blocked_reason,
            "repair_contract": self.repair_contract.to_dict() if self.repair_contract else None,
        }


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
    ) -> "RepairReplanSignal":
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


class FailureAnalyzer:
    def __init__(self, *, model_runner: Any, trace: Any | None = None) -> None:
        self.model_runner = model_runner
        self.trace = trace

    def analyze(self, request: FailureAnalysisRequest) -> FailureAnalysisResult:
        self._record("requested", request=request)
        model_request = self._model_request(request)
        result = self.model_runner.run_turn(model_request)
        if result.status != ModelTurnStatus.SUCCESS or result.assistant_message is None:
            reason = (
                result.error.message
                if result.error is not None
                else "failure_analysis_model_request_failed"
            )
            self._record("failed", request=request, error=reason)
            return FailureAnalysisResult.blocked(request=request, reason=reason)
        try:
            payload = _json_payload(result.assistant_message.text)
        except ValueError as exc:
            self._record("failed", request=request, error=str(exc))
            return FailureAnalysisResult.blocked(
                request=request,
                reason=f"failure_analysis_invalid_json: {exc}",
                category="failure_analysis_invalid_json",
            )
        try:
            analysis = FailureAnalysisResult.from_model_payload(
                payload,
                request=request,
                raw_response_ref=result.raw_response_ref,
            )
        except ValueError as exc:
            self._record("failed", request=request, error=str(exc))
            return FailureAnalysisResult.blocked(
                request=request,
                reason=f"failure_analysis_schema_invalid: {exc}",
                category="failure_analysis_schema_invalid",
            )
        self._record("completed", request=request, analysis=analysis)
        return analysis

    def _model_request(self, request: FailureAnalysisRequest) -> ModelTurnRequest:
        payload = request.to_model_payload()
        prompt = (
            "Analyze this structured failure summary and produce a repair plan. "
            "Use only the supplied summaries, references, recent tail, verification "
            "log refs, and changed file names. Do not ask for raw logs unless a "
            "reference is missing. Return JSON only with keys: root_cause, "
            "failure_category, affected_files, evidence_refs, repair_strategy, "
            "next_actions, verification_plan, confidence, needs_user_input, "
            "blocked_reason.\n\n"
            + json.dumps(payload, ensure_ascii=False, sort_keys=True, default=str)
        )
        return ModelTurnRequest(
            request_id=request.request_id,
            run_id=request.run_id,
            session_id=request.session_id,
            task_id=request.task_id,
            phase_id="failure_analysis",
            action_id=request.request_id,
            purpose=ModelPurpose.FAILURE_ANALYSIS,
            messages=[
                ModelMessage(
                    role=ModelRole.USER,
                    content=[ContentBlock.from_text(prompt)],
                )
            ],
            tools=[],
            tool_choice=ToolChoicePolicy(mode=ToolChoiceMode.NONE, max_tool_calls=0),
            model_preferences=ModelPreferences(json_mode=True, max_output_tokens=1200),
            budget=ModelBudget(max_retries=1, max_output_tokens=1200),
            context_metadata={
                "input_policy": "structured_failure_summaries_only",
                "repair_planning": True,
            },
        )

    def _record(
        self,
        status: str,
        *,
        request: FailureAnalysisRequest,
        analysis: FailureAnalysisResult | None = None,
        error: str | None = None,
    ) -> None:
        event = f"failure_analysis_{status}"
        payload: dict[str, Any] = {
            "request_id": request.request_id,
            "failure_source": request.failure_source,
            "evidence_refs": request.evidence_refs,
            "changed_files": request.changed_files,
        }
        if analysis is not None:
            payload["analysis"] = analysis.to_dict()
        if error:
            payload["error"] = error
        if self.trace is None:
            return
        if hasattr(self.trace, "emit"):
            event_type = {
                "requested": TraceEventType.FAILURE_ANALYSIS_REQUESTED,
                "completed": TraceEventType.FAILURE_ANALYSIS_COMPLETED,
                "failed": TraceEventType.FAILURE_ANALYSIS_FAILED,
            }[status]
            self.trace.emit(
                event_type,
                component="failure_analysis",
                summary=f"Failure analysis {status}.",
                payload=payload,
                ids={
                    "run_id": request.run_id,
                    "session_id": request.session_id,
                    "task_id": request.task_id,
                    "phase_id": "failure_analysis",
                    "action_id": request.request_id,
                },
                severity=TraceSeverity.ERROR if status == "failed" else TraceSeverity.INFO,
            )
        elif hasattr(self.trace, "record"):
            self.trace.record(event, payload)


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


def _failure_sources(evidence: Any, *, outcome: ExecutionOutcome | None) -> list[dict[str, Any]]:
    sources: list[dict[str, Any]] = []
    if outcome is not None:
        sources.append({"kind": "execution_outcome", **_safe_outcome(outcome)})
    if evidence is None:
        return sources
    for item in list(getattr(evidence, "tool_results", []) or [])[-5:]:
        if item.get("ok") is False or item.get("error_code"):
            sources.append({"kind": "tool_result", **_trim_dict(item)})
    for item in list(getattr(evidence, "command_results", []) or [])[-5:]:
        if item.get("semantic_status") not in {None, "succeeded", "SUCCEEDED"}:
            sources.append({"kind": "command_observation", **_command_summary(item)})
    for item in list(getattr(evidence, "edit_results", []) or [])[-5:]:
        if item.get("error_code") or item.get("status") in {"failed", "blocked"}:
            sources.append({"kind": "edit_result", **_trim_dict(item)})
    latest_verification = (
        getattr(evidence, "verification_results", [])[-1]
        if getattr(evidence, "verification_results", None)
        else None
    )
    if isinstance(latest_verification, dict):
        assessment = latest_verification.get("completion_assessment") or {}
        if assessment.get("status") in {"failed", "blocked", "needs_review"}:
            sources.append(
                {
                    "kind": "verification_assessment",
                    "completion_assessment": _trim_dict(assessment),
                    "check_status": latest_verification.get("check_status") or [],
                }
            )
        for result in latest_verification.get("results") or []:
            if not isinstance(result, dict) or result.get("status") not in {
                "failed",
                "blocked",
                "timeout",
                "flaky",
            }:
                continue
            result_evidence = result.get("evidence") or {}
            sources.append(
                {
                    "kind": "verification_result",
                    "check_id": result.get("check_id"),
                    "status": result.get("status"),
                    "failure_type": result.get("failure_type"),
                    "evidence": _verification_evidence_summary(result_evidence),
                    "repair_hints": result.get("repair_hints") or [],
                }
            )
    for item in list(getattr(evidence, "review_results", []) or [])[-3:]:
        decision = item.get("decision") if isinstance(item, dict) else {}
        if isinstance(decision, dict) and decision.get("action") in {
            "repair",
            "reject",
            "needs_human_approval",
        }:
            sources.append({"kind": "review_observation", **_trim_dict(item)})
    for item in list(getattr(evidence, "unresolved_failures", []) or [])[-5:]:
        sources.append({"kind": "unresolved_failure", **_trim_dict(item)})
    return sources[-12:]


def _failure_summary(
    sources: list[dict[str, Any]],
    *,
    outcome: ExecutionOutcome | None,
) -> str:
    if outcome is not None and (outcome.observation_summary or outcome.reason):
        return _limit(outcome.observation_summary or outcome.reason, SUMMARY_LIMIT)
    if not sources:
        return ""
    first = sources[-1]
    return _limit(str(first.get("failure_type") or first.get("error_code") or first), SUMMARY_LIMIT)


def _fingerprint_sources(sources: list[dict[str, Any]]) -> list[dict[str, Any]]:
    fingerprints: list[dict[str, Any]] = []
    seen: set[str] = set()
    for source in sources:
        item = _fingerprint_source(source)
        key = json.dumps(item, ensure_ascii=False, sort_keys=True, default=str)
        if key in seen:
            continue
        seen.add(key)
        fingerprints.append(item)
    return fingerprints


def _fingerprint_source(source: dict[str, Any]) -> dict[str, Any]:
    evidence_raw = source.get("evidence")
    evidence: dict[str, Any] = evidence_raw if isinstance(evidence_raw, dict) else {}
    assessment_raw = source.get("completion_assessment")
    assessment: dict[str, Any] = assessment_raw if isinstance(assessment_raw, dict) else {}
    parsed_messages: list[str] = []
    for parsed in evidence.get("parsed_failures") or []:
        if isinstance(parsed, dict):
            _append_unique(parsed_messages, parsed.get("message"))
    repair_targets: list[str] = []
    for path in _paths_from_failure_source(source):
        _append_unique(repair_targets, _normalize_workspace_path(path, workspace_root=""))
    return {
        "kind": source.get("kind"),
        "outcome_ref": source.get("outcome_ref"),
        "status": source.get("status") or assessment.get("status"),
        "error_code": source.get("error_code"),
        "failure_type": source.get("failure_type"),
        "exit_code": source.get("exit_code") or evidence.get("exit_code"),
        "command_preview": _stable_command_preview(
            source.get("command_preview") or evidence.get("command")
        ),
        "parsed_messages": parsed_messages[:5],
        "repair_targets": repair_targets[:5],
    }


def _stable_command_preview(value: Any) -> str | None:
    if value is None:
        return None
    if isinstance(value, list | tuple):
        return " ".join(str(item) for item in value)
    text = str(value)
    text = text.replace("\\", "/")
    return re.sub(r"[A-Za-z]:/[^\\s\"']+", "<path>", text)


def _safe_outcome(outcome: ExecutionOutcome) -> dict[str, Any]:
    payload = outcome.to_dict()
    payload["outcome_ref"] = f"execution_outcome:{payload.get('error_code') or payload.get('status')}"
    payload["metadata"] = _trim_dict(payload.get("metadata") or {})
    return payload


def _command_summary(command: dict[str, Any]) -> dict[str, Any]:
    return {
        "command_id": command.get("command_id"),
        "command_preview": command.get("shell")
        or " ".join(str(item) for item in command.get("argv") or []),
        "exit_code": command.get("exit_code"),
        "status": command.get("semantic_status") or command.get("execution_status"),
        "stdout_preview": _limit(command.get("stdout_excerpt") or command.get("stdout") or "", SUMMARY_LIMIT),
        "stderr_preview": _limit(command.get("stderr_excerpt") or command.get("stderr") or "", SUMMARY_LIMIT),
        "output_ref": command.get("artifact_path") or command.get("output_ref"),
        "policy_decision_id": command.get("policy_decision_id"),
        "parsed_failures": command.get("parsed_failures") or [],
    }


def _verification_evidence_summary(evidence: dict[str, Any]) -> dict[str, Any]:
    return {
        "command_id": evidence.get("command_id"),
        "command": evidence.get("command"),
        "exit_code": evidence.get("exit_code"),
        "output_excerpt": _limit(evidence.get("output_excerpt") or "", SUMMARY_LIMIT),
        "stdout_excerpt": _limit(evidence.get("stdout_excerpt") or "", SUMMARY_LIMIT),
        "stderr_excerpt": _limit(evidence.get("stderr_excerpt") or "", SUMMARY_LIMIT),
        "artifact_ref": evidence.get("artifact_ref") or evidence.get("artifact_path"),
        "parsed_failures": (evidence.get("parsed_failures") or [])[:8],
        "sandbox_status": evidence.get("sandbox_status"),
        "sandbox_violations": evidence.get("sandbox_violations") or [],
        "capability_summary": evidence.get("capability_summary") or {},
    }


def _context_references(context: Any, evidence: Any) -> list[str]:
    refs: list[str] = []
    for observation in list(getattr(context, "tool_observations", []) or [])[-8:]:
        _append_unique(refs, getattr(observation, "id", None))
        for ref in getattr(observation, "source_refs", []) or []:
            _append_unique(refs, getattr(ref, "ref_id", None))
    if evidence is not None:
        for ref in _evidence_refs(evidence):
            _append_unique(refs, ref)
    return refs


def _recent_tail(context: Any) -> list[dict[str, Any]]:
    tail: list[dict[str, Any]] = []
    for observation in list(getattr(context, "tool_observations", []) or [])[-6:]:
        tail.append(
            {
                "source": "tool_observation",
                "tool_name": getattr(observation, "tool_name", None),
                "tool_call_id": getattr(observation, "tool_call_id", None),
                "ok": getattr(observation, "ok", None),
                "error_code": getattr(observation, "error_code", None),
                "preview": _limit(getattr(observation, "preview", "") or "", TAIL_LIMIT),
            }
        )
    try:
        messages = context.messages(persist=False)
    except Exception:
        messages = []
    for message in list(messages or [])[-4:]:
        tail.append(
            {
                "source": "message",
                "role": message.get("role"),
                "tool_call_id": message.get("tool_call_id"),
                "content_preview": _limit(message.get("content") or "", TAIL_LIMIT),
            }
        )
    return tail[-8:]


def _verification_log_refs(evidence: Any) -> list[str]:
    refs: list[str] = []
    if evidence is None:
        return refs
    for verification in list(getattr(evidence, "verification_results", []) or [])[-3:]:
        if not isinstance(verification, dict):
            continue
        for result in verification.get("results") or []:
            evidence_payload = result.get("evidence") if isinstance(result, dict) else {}
            if isinstance(evidence_payload, dict):
                _append_unique(refs, evidence_payload.get("artifact_ref"))
                _append_unique(refs, evidence_payload.get("artifact_path"))
                _append_unique(refs, evidence_payload.get("command_id"))
    return refs


def _changed_files(evidence: Any) -> list[str]:
    changed: list[str] = []
    if evidence is None:
        return changed
    for change in getattr(evidence, "applied_changes", []) or []:
        for path in change.get("changed_files") or []:
            _append_unique(changed, path)
    return changed


def _evidence_refs(evidence: Any) -> list[str]:
    refs: list[str] = []
    if evidence is None:
        return refs
    for result in getattr(evidence, "tool_results", []) or []:
        _append_unique(refs, result.get("tool_call_id"))
    for command in getattr(evidence, "command_results", []) or []:
        _append_unique(refs, command.get("command_id"))
    for verification in getattr(evidence, "verification_results", []) or []:
        for status in verification.get("check_status") or []:
            _append_unique(refs, status.get("check_id"))
    for review in getattr(evidence, "review_results", []) or []:
        _append_unique(refs, review.get("review_id"))
    for change in getattr(evidence, "applied_changes", []) or []:
        _append_unique(refs, change.get("transaction_id"))
        _append_unique(refs, change.get("changeset_id"))
    return refs


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


def _json_payload(text: str) -> dict[str, Any]:
    try:
        value = json.loads(text)
    except json.JSONDecodeError:
        match = re.search(r"\{.*\}", text, flags=re.DOTALL)
        if not match:
            raise ValueError("model response did not contain a JSON object")
        value = json.loads(match.group(0))
    if not isinstance(value, dict):
        raise ValueError("model response JSON was not an object")
    return value


def _required_text(payload: dict[str, Any], field_name: str) -> str:
    value = payload.get(field_name)
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"{field_name} must be a non-empty string")
    return _limit(value.strip(), SUMMARY_LIMIT)


def _bool_required(payload: dict[str, Any], field_name: str) -> bool:
    value = payload.get(field_name)
    if not isinstance(value, bool):
        raise ValueError(f"{field_name} must be a boolean")
    return value


def _strings_required(payload: dict[str, Any], field_name: str) -> list[str]:
    if field_name not in payload:
        raise ValueError(f"{field_name} is required")
    values = _strings(payload.get(field_name))
    if not values:
        raise ValueError(f"{field_name} must contain at least one item")
    return [_limit(item.strip(), SUMMARY_LIMIT) for item in values if item.strip()]


def _confidence_required(value: Any) -> float:
    if isinstance(value, bool):
        raise ValueError("confidence must be numeric")
    try:
        confidence = float(value)
    except (TypeError, ValueError) as exc:
        raise ValueError("confidence must be numeric") from exc
    if confidence < 0.0 or confidence > 1.0:
        raise ValueError("confidence must be between 0 and 1")
    return confidence


def _validate_evidence_refs(refs: list[str], *, request: FailureAnalysisRequest) -> None:
    known = set(request.evidence_refs) | set(request.context_references) | set(request.verification_log_refs)
    known.update(request.failure_evidence_refs)
    if not refs:
        raise ValueError("evidence_refs must not be empty")
    if known and not any(ref in known for ref in refs):
        raise ValueError("evidence_refs must reference supplied failure evidence")


def _validated_affected_files(value: Any, *, request: FailureAnalysisRequest) -> list[str]:
    raw_paths = _strings(value)
    allowed = {path for path in request.allowed_target_files if path}
    if not raw_paths:
        return []
    resolved: list[str] = []
    for raw_path in raw_paths:
        normalized = _normalize_workspace_path(raw_path, workspace_root=request.workspace_root)
        if not normalized:
            raise ValueError(f"affected_files contains an invalid workspace path: {raw_path}")
        if allowed and normalized not in allowed:
            raise ValueError(f"affected_files contains unauthorized target: {raw_path}")
        _append_unique(resolved, normalized)
    return resolved


def _validate_verification_plan(
    plan: list[str],
    *,
    needs_user_input: bool,
    verification_contract: VerificationContract | None = None,
) -> None:
    if needs_user_input:
        return
    has_contract = (
        verification_contract is not None
        and verification_contract.is_valid
    )
    if not plan and not has_contract:
        raise ValueError("verification_plan must contain at least one executable verification step")
    if verification_contract is not None and verification_contract.validation_errors:
        raise ValueError(
            "verification_contract invalid: " + "; ".join(verification_contract.validation_errors)
        )
    for item in plan:
        text = item.strip()
        if not text:
            raise ValueError("verification_plan contains an empty step")
        if text in INTERNAL_VERIFICATION_REFS:
            continue
        if len(text.split()) < 2:
            raise ValueError(f"verification_plan step is not executable enough: {text}")


def _repair_contract_errors(
    *,
    analysis: FailureAnalysisResult,
    action_candidates: list[RepairActionCandidate],
    allowed_tool_names: list[str],
    verification_contract: VerificationContract | None = None,
) -> list[str]:
    errors: list[str] = []
    if not FAILURE_CATEGORY_PATTERN.match(analysis.failure_category):
        errors.append("invalid_failure_category")
    if analysis.confidence < MIN_REPAIR_CONFIDENCE:
        errors.append("low_confidence")
    if not analysis.needs_user_input and not analysis.evidence_refs:
        errors.append("missing_evidence_refs")
    if not analysis.needs_user_input and not analysis.affected_files:
        errors.append("missing_target_files")
    if not analysis.needs_user_input and not action_candidates:
        errors.append("missing_action_candidates")
    for candidate in action_candidates:
        errors.extend(_repair_action_candidate_errors(candidate, target_files=analysis.affected_files))
    if not analysis.needs_user_input:
        try:
            _validate_verification_plan(
                analysis.verification_plan,
                needs_user_input=False,
                verification_contract=verification_contract,
            )
        except ValueError as exc:
            errors.append(str(exc))
    if verification_contract is not None and verification_contract.validation_errors:
        errors.extend(verification_contract.validation_errors)
    if any(tool not in TOOL_HINTS for tool in allowed_tool_names):
        errors.append("unsupported_tool_hint")
    return sorted(dict.fromkeys(errors))


def _repair_action_candidate_errors(
    candidate: RepairActionCandidate,
    *,
    target_files: list[str],
) -> list[str]:
    errors: list[str] = []
    if candidate.action_type not in ACTION_TYPES:
        errors.append(f"invalid_action_type:{candidate.action_type}")
    if not candidate.rationale.strip():
        errors.append("missing_action_rationale")
    if candidate.confidence < 0.0 or candidate.confidence > 1.0:
        errors.append("invalid_action_confidence")
    for tool in candidate.tool_hints:
        if tool not in TOOL_HINTS:
            errors.append(f"unsupported_tool_hint:{tool}")
    if candidate.action_type in {"edit", "inspect"} and not candidate.target_file:
        errors.append("missing_target_file")
    if candidate.target_file and target_files and candidate.target_file not in target_files:
        errors.append(f"unauthorized_candidate_target:{candidate.target_file}")
    return errors


def _allowed_tools_from_candidates(candidates: list[RepairActionCandidate]) -> list[str]:
    allowed: list[str] = []
    for candidate in candidates:
        for tool in candidate.tool_hints:
            _append_unique(allowed, tool)
    return allowed


def _paths_from_failure_source(source: dict[str, Any]) -> list[str]:
    paths: list[str] = []
    _append_unique(paths, source.get("target_file"))
    for key in ("affected_files", "changed_files", "suspect_files"):
        for path in source.get(key) or []:
            _append_unique(paths, path)
    evidence = source.get("evidence")
    if isinstance(evidence, dict):
        for parsed in evidence.get("parsed_failures") or []:
            if isinstance(parsed, dict):
                _append_unique(paths, parsed.get("file"))
        for path in evidence.get("sandbox_changed_files") or []:
            _append_unique(paths, path)
    for hint in source.get("repair_hints") or []:
        if isinstance(hint, dict):
            _append_unique(paths, hint.get("target_file"))
    for status in source.get("check_status") or []:
        if isinstance(status, dict):
            _append_unique(paths, status.get("file"))
    return paths


def _normalize_workspace_path(path: Any, *, workspace_root: str) -> str | None:
    text = str(path or "").strip().replace("\\", "/")
    if not text:
        return None
    if text.startswith("workspace:"):
        text = text.removeprefix("workspace:")
    if text.startswith("file://"):
        text = text.removeprefix("file://")
    candidate = Path(text)
    if candidate.is_absolute():
        if not workspace_root:
            return None
        try:
            return candidate.resolve(strict=False).relative_to(Path(workspace_root).resolve(strict=False)).as_posix()
        except ValueError:
            return None
    normalized = Path(text).as_posix()
    if normalized.startswith("../") or normalized == ".." or "/../" in normalized or normalized.startswith("/"):
        return None
    return normalized


def _trim_dict(value: dict[str, Any]) -> dict[str, Any]:
    return {
        str(key): _limit(item, SUMMARY_LIMIT) if isinstance(item, str) else item
        for key, item in value.items()
        if key not in {"raw_output", "raw_stdout", "raw_stderr", "content"}
    }


def _strings(value: Any) -> list[str]:
    if value is None:
        return []
    if isinstance(value, str):
        return [value]
    if isinstance(value, list | tuple | set):
        return [str(item) for item in value if item is not None]
    return [str(value)]


def _text(value: Any) -> str:
    return _limit(str(value or ""), SUMMARY_LIMIT)


def _confidence(value: Any) -> float:
    try:
        return max(0.0, min(1.0, float(value)))
    except (TypeError, ValueError):
        return 0.5


def _limit(value: Any, limit: int) -> str:
    text = str(value or "")
    return text if len(text) <= limit else text[:limit] + "...[truncated]"


def _append_unique(values: list[str], value: Any) -> None:
    if value is None:
        return
    text = str(value)
    if text and text not in values:
        values.append(text)
