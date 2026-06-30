from __future__ import annotations

from dataclasses import dataclass, field
from datetime import UTC, datetime
from enum import StrEnum
from typing import Any, ClassVar
from uuid import uuid4


class TaskStatus(StrEnum):
    INITIALIZED = "initialized"
    UNDERSTANDING_TASK = "understanding_task"
    INSPECTING_WORKSPACE = "inspecting_workspace"
    PLANNING_CHANGES = "planning_changes"
    APPLYING_CHANGES = "applying_changes"
    RUNNING_VERIFICATION = "running_verification"
    REPAIRING_FAILURES = "repairing_failures"
    FINALIZING = "finalizing"
    COMPLETED = "completed"
    BLOCKED = "blocked"
    FAILED = "failed"
    NEEDS_REVIEW = "needs_review"
    INTERRUPTED = "interrupted"
    RECOVERING = "recovering"


class ActionKind(StrEnum):
    INSPECT_WORKSPACE = "InspectWorkspace"
    READ_RELEVANT_FILES = "ReadRelevantFiles"
    SEARCH_CODE = "SearchCode"
    ANALYZE_ISSUE = "AnalyzeIssue"
    PROPOSE_CHANGE_SET = "ProposeChangeSet"
    APPLY_MUTATION = "ApplyMutation"
    RUN_VERIFICATION = "RunVerification"
    PARSE_FAILURE = "ParseFailure"
    REPAIR_CHANGE = "RepairChange"
    ASK_USER = "AskUser"
    REQUIRE_REVIEW = "RequireReview"
    FINALIZE = "Finalize"
    ABORT = "Abort"


class ActionStatus(StrEnum):
    PROPOSED = "proposed"
    ALLOWED = "allowed"
    RUNNING = "running"
    SUCCEEDED = "succeeded"
    FAILED = "failed"
    BLOCKED = "blocked"


class RiskLevel(StrEnum):
    LOW = "low"
    MEDIUM = "medium"
    HIGH = "high"
    CRITICAL = "critical"


class RiskDecisionKind(StrEnum):
    CONTINUE = "continue"
    REQUIRE_REVIEW = "require_review"
    ASK_USER = "ask_user"
    DENY_ACTION = "deny_action"
    ABORT = "abort"


class ReplanDecisionKind(StrEnum):
    CONTINUE = "continue"
    RETRY_WITH_NEW_CONTEXT = "retry_with_new_context"
    READ_FRESH_FILE = "read_fresh_file"
    REPAIR_FAILURE = "repair_failure"
    RERUN_VERIFICATION = "rerun_verification"
    ASK_USER = "ask_user"
    REQUIRE_REVIEW = "require_review"
    ABORT = "abort"
    FINALIZE_WITH_WARNINGS = "finalize_with_warnings"


PLANNER_ERROR_CODES = {
    "task_parse_failed",
    "plan_generation_failed",
    "invalid_phase_transition",
    "action_not_allowed",
    "missing_required_evidence",
    "completion_criteria_unmet",
    "repeated_failure",
    "repair_budget_exceeded",
    "risk_escalated",
    "needs_review",
    "blocked_by_workspace_conflict",
    "blocked_by_verification",
    "context_render_failed",
    "resume_failed",
    "finalization_failed",
    "internal_error",
}


@dataclass
class CompletionCriteria:
    required_files_inspected: bool = True
    required_changes_applied: bool = True
    required_verifications_passed: bool = True
    unresolved_failures_empty: bool = True
    workspace_health_acceptable: bool = True
    risks_acknowledged: bool = True
    final_report_ready: bool = False

    def to_dict(self) -> dict[str, Any]:
        return self.__dict__.copy()

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> CompletionCriteria:
        return cls(**payload)


@dataclass
class TaskState:
    task_id: str
    session_id: str
    user_goal: str
    normalized_goal: str
    effective_goal: str | None = None
    goal_revisions: list[dict[str, Any]] = field(default_factory=list)
    constraints: list[str] = field(default_factory=list)
    assumptions: list[str] = field(default_factory=list)
    current_phase: str = "understanding_task"
    status: TaskStatus = TaskStatus.INITIALIZED
    risk_level: RiskLevel = RiskLevel.LOW
    created_at: str = field(default_factory=lambda: _now())
    updated_at: str = field(default_factory=lambda: _now())
    completion_criteria: CompletionCriteria = field(default_factory=CompletionCriteria)
    open_questions: list[str] = field(default_factory=list)
    blocked_reasons: list[str] = field(default_factory=list)
    linked_transactions: list[str] = field(default_factory=list)
    linked_commands: list[str] = field(default_factory=list)
    linked_verifications: list[str] = field(default_factory=list)
    final_assessment: dict[str, Any] = field(default_factory=dict)
    task_contract: dict[str, Any] = field(default_factory=dict)
    lifecycle_status: str = "created"
    rolling_plan: dict[str, Any] = field(default_factory=dict)
    sandbox_capability: dict[str, Any] = field(default_factory=dict)
    # Semantic Planner capability layer: structured risk/verification/repair
    # policy produced by the model-driven producers. Stored as dicts for
    # TaskState serialization compatibility; producers hydrate to objects
    # via semantic_objects.{RiskPoint,VerificationStrategy,RepairPolicy}.from_dict.
    risk_points: list[dict[str, Any]] = field(default_factory=list)
    verification_strategies: list[dict[str, Any]] = field(default_factory=list)
    repair_policy: dict[str, Any] | None = None

    def touch(self) -> None:
        self.updated_at = _now()

    def to_dict(self) -> dict[str, Any]:
        return {
            "task_id": self.task_id,
            "session_id": self.session_id,
            "user_goal": self.user_goal,
            "normalized_goal": self.normalized_goal,
            "effective_goal": self.effective_goal or self.normalized_goal,
            "goal_revisions": self.goal_revisions,
            "constraints": self.constraints,
            "assumptions": self.assumptions,
            "current_phase": self.current_phase,
            "status": self.status.value,
            "risk_level": self.risk_level.value,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "completion_criteria": self.completion_criteria.to_dict(),
            "open_questions": self.open_questions,
            "blocked_reasons": self.blocked_reasons,
            "linked_transactions": self.linked_transactions,
            "linked_commands": self.linked_commands,
            "linked_verifications": self.linked_verifications,
            "final_assessment": self.final_assessment,
            "task_contract": self.task_contract,
            "lifecycle_status": self.lifecycle_status,
            "rolling_plan": self.rolling_plan,
            "sandbox_capability": self.sandbox_capability,
            "risk_points": self.risk_points,
            "verification_strategies": self.verification_strategies,
            "repair_policy": self.repair_policy,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> TaskState:
        return cls(
            task_id=str(payload["task_id"]),
            session_id=str(payload["session_id"]),
            user_goal=str(payload["user_goal"]),
            normalized_goal=str(payload["normalized_goal"]),
            effective_goal=str(payload.get("effective_goal") or payload["normalized_goal"]),
            goal_revisions=list(payload.get("goal_revisions") or []),
            constraints=list(payload.get("constraints") or []),
            assumptions=list(payload.get("assumptions") or []),
            current_phase=str(payload.get("current_phase") or "understanding_task"),
            status=TaskStatus(payload.get("status") or TaskStatus.INITIALIZED.value),
            risk_level=RiskLevel(payload.get("risk_level") or RiskLevel.LOW.value),
            created_at=str(payload.get("created_at") or _now()),
            updated_at=str(payload.get("updated_at") or _now()),
            completion_criteria=CompletionCriteria.from_dict(
                payload.get("completion_criteria") or {}
            ),
            open_questions=list(payload.get("open_questions") or []),
            blocked_reasons=list(payload.get("blocked_reasons") or []),
            linked_transactions=list(payload.get("linked_transactions") or []),
            linked_commands=list(payload.get("linked_commands") or []),
            linked_verifications=list(payload.get("linked_verifications") or []),
            final_assessment=dict(payload.get("final_assessment") or {}),
            task_contract=dict(payload.get("task_contract") or {}),
            lifecycle_status=str(payload.get("lifecycle_status") or "created"),
            rolling_plan=dict(payload.get("rolling_plan") or {}),
            sandbox_capability=dict(payload.get("sandbox_capability") or {}),
            risk_points=list(payload.get("risk_points") or []),
            verification_strategies=list(payload.get("verification_strategies") or []),
            repair_policy=(
                dict(repair_policy_raw)
                if (repair_policy_raw := payload.get("repair_policy"))
                else None
            ),
        )


@dataclass
class TaskPhase:
    phase_id: str
    name: str
    purpose: str
    allowed_tools: list[str]
    allowed_actions: list[ActionKind]
    entry_conditions: list[str] = field(default_factory=list)
    exit_conditions: list[str] = field(default_factory=list)
    required_evidence: list[str] = field(default_factory=list)
    failure_policy: str = "replan_or_block"
    risk_notes: list[str] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {
            "phase_id": self.phase_id,
            "name": self.name,
            "purpose": self.purpose,
            "allowed_tools": self.allowed_tools,
            "allowed_actions": [action.value for action in self.allowed_actions],
            "entry_conditions": self.entry_conditions,
            "exit_conditions": self.exit_conditions,
            "required_evidence": self.required_evidence,
            "failure_policy": self.failure_policy,
            "risk_notes": self.risk_notes,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> TaskPhase:
        return cls(
            phase_id=str(payload["phase_id"]),
            name=str(payload["name"]),
            purpose=str(payload["purpose"]),
            allowed_tools=list(payload.get("allowed_tools") or []),
            allowed_actions=[
                ActionKind(action) for action in (payload.get("allowed_actions") or [])
            ],
            entry_conditions=list(payload.get("entry_conditions") or []),
            exit_conditions=list(payload.get("exit_conditions") or []),
            required_evidence=list(payload.get("required_evidence") or []),
            failure_policy=str(payload.get("failure_policy") or "replan_or_block"),
            risk_notes=list(payload.get("risk_notes") or []),
        )


@dataclass
class TaskPlan:
    plan_id: str
    task_id: str
    phases: list[TaskPhase]
    current_phase: str
    version: int = 1
    updated_at: str = field(default_factory=lambda: _now())

    def phase(self, phase_id: str | None = None) -> TaskPhase:
        resolved = phase_id or self.current_phase
        for phase in self.phases:
            if phase.phase_id == resolved:
                return phase
        raise KeyError(f"Unknown task phase: {resolved}")

    def next_phase_id(self) -> str | None:
        for index, phase in enumerate(self.phases):
            if phase.phase_id == self.current_phase and index + 1 < len(self.phases):
                return self.phases[index + 1].phase_id
        return None

    def to_dict(self) -> dict[str, Any]:
        return {
            "plan_id": self.plan_id,
            "task_id": self.task_id,
            "phases": [phase.to_dict() for phase in self.phases],
            "current_phase": self.current_phase,
            "version": self.version,
            "updated_at": self.updated_at,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> TaskPlan:
        return cls(
            plan_id=str(payload["plan_id"]),
            task_id=str(payload["task_id"]),
            phases=[TaskPhase.from_dict(item) for item in payload.get("phases") or []],
            current_phase=str(payload.get("current_phase") or "understanding_task"),
            version=int(payload.get("version") or 1),
            updated_at=str(payload.get("updated_at") or _now()),
        )


@dataclass
class AgentAction:
    kind: ActionKind
    intent: str
    phase_id: str
    preconditions: list[str]
    allowed_tools: list[str]
    expected_evidence: list[str]
    risk_level: RiskLevel = RiskLevel.LOW
    status: ActionStatus = ActionStatus.PROPOSED
    action_id: str = field(default_factory=lambda: f"action_{uuid4().hex[:12]}")
    result_ref: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "action_id": self.action_id,
            "kind": self.kind.value,
            "intent": self.intent,
            "phase_id": self.phase_id,
            "preconditions": self.preconditions,
            "allowed_tools": self.allowed_tools,
            "expected_evidence": self.expected_evidence,
            "risk_level": self.risk_level.value,
            "status": self.status.value,
            "result_ref": self.result_ref,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> AgentAction:
        return cls(
            action_id=str(payload.get("action_id") or f"action_{uuid4().hex[:12]}"),
            kind=ActionKind(payload["kind"]),
            intent=str(payload.get("intent") or ""),
            phase_id=str(payload.get("phase_id") or ""),
            preconditions=list(payload.get("preconditions") or []),
            allowed_tools=list(payload.get("allowed_tools") or []),
            expected_evidence=list(payload.get("expected_evidence") or []),
            risk_level=RiskLevel(payload.get("risk_level") or RiskLevel.LOW.value),
            status=ActionStatus(payload.get("status") or ActionStatus.PROPOSED.value),
            result_ref=payload.get("result_ref"),
        )


@dataclass(frozen=True)
class VerificationEvidenceRecord:
    completion_assessment: dict[str, Any] = field(default_factory=dict)
    check_status: list[dict[str, Any]] = field(default_factory=list)
    results: list[dict[str, Any]] = field(default_factory=list)
    tool_call_id: str | None = None
    plan: dict[str, Any] = field(default_factory=dict)
    extra: dict[str, Any] = field(default_factory=dict)

    @property
    def completion_status(self) -> str | None:
        status = self.completion_assessment.get("status")
        return str(status) if status is not None else None

    def to_dict(self) -> dict[str, Any]:
        payload = dict(self.extra)
        if self.completion_assessment:
            payload["completion_assessment"] = dict(self.completion_assessment)
        if self.check_status:
            payload["check_status"] = list(self.check_status)
        if self.results:
            payload["results"] = list(self.results)
        if self.tool_call_id is not None:
            payload["tool_call_id"] = self.tool_call_id
        if self.plan:
            payload["plan"] = dict(self.plan)
        return payload

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> VerificationEvidenceRecord:
        known = {"completion_assessment", "check_status", "results", "tool_call_id", "plan"}
        return cls(
            completion_assessment=dict(payload.get("completion_assessment") or {}),
            check_status=[dict(item) for item in payload.get("check_status") or [] if isinstance(item, dict)],
            results=[dict(item) for item in payload.get("results") or [] if isinstance(item, dict)],
            tool_call_id=(
                str(payload["tool_call_id"])
                if payload.get("tool_call_id") is not None
                else None
            ),
            plan=dict(payload.get("plan") or {}),
            extra={key: value for key, value in payload.items() if key not in known},
        )


@dataclass(frozen=True)
class SandboxObservationRecord:
    source: str | None = None
    backend: str | None = None
    status: str | None = None
    enforcement_status: str | None = None
    execution_backend: str | None = None
    network_denied_verified: bool | None = None
    process_tree_kill: bool | None = None
    job_killed: bool | None = None
    timeout_enforced: bool | None = None
    artifact_count: int = 0
    artifact_refs: list[str] = field(default_factory=list)
    changed_files_count: int = 0
    violations: list[dict[str, Any]] = field(default_factory=list)
    imported_changes_count: int = 0
    extra: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        payload = dict(self.extra)
        payload.update(
            {
                "source": self.source,
                "backend": self.backend,
                "status": self.status,
                "enforcement_status": self.enforcement_status,
                "execution_backend": self.execution_backend,
                "network_denied_verified": self.network_denied_verified,
                "process_tree_kill": self.process_tree_kill,
                "job_killed": self.job_killed,
                "timeout_enforced": self.timeout_enforced,
                "artifact_count": self.artifact_count,
                "artifact_refs": list(self.artifact_refs),
                "changed_files_count": self.changed_files_count,
                "violations": list(self.violations),
                "imported_changes_count": self.imported_changes_count,
            }
        )
        return payload

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> SandboxObservationRecord:
        known = {
            "source",
            "backend",
            "status",
            "enforcement_status",
            "execution_backend",
            "network_denied_verified",
            "process_tree_kill",
            "job_killed",
            "timeout_enforced",
            "artifact_count",
            "artifact_refs",
            "changed_files_count",
            "violations",
            "imported_changes_count",
        }
        return cls(
            source=_optional_str(payload.get("source")),
            backend=_optional_str(payload.get("backend")),
            status=_optional_str(payload.get("status")),
            enforcement_status=_optional_str(payload.get("enforcement_status")),
            execution_backend=_optional_str(payload.get("execution_backend")),
            network_denied_verified=_optional_bool(payload.get("network_denied_verified")),
            process_tree_kill=_optional_bool(payload.get("process_tree_kill")),
            job_killed=_optional_bool(payload.get("job_killed")),
            timeout_enforced=_optional_bool(payload.get("timeout_enforced")),
            artifact_count=int(payload.get("artifact_count") or 0),
            artifact_refs=[str(item) for item in payload.get("artifact_refs") or []],
            changed_files_count=int(payload.get("changed_files_count") or 0),
            violations=[dict(item) for item in payload.get("violations") or [] if isinstance(item, dict)],
            imported_changes_count=int(payload.get("imported_changes_count") or 0),
            extra={key: value for key, value in payload.items() if key not in known},
        )


@dataclass(frozen=True)
class PolicyObservationRecord:
    outcome: str | None = None
    component: str | None = None
    operation: str | None = None
    reason: str | None = None
    risk_level: str | None = None
    resource: str | None = None
    approval_grant_id: str | None = None
    approved_by_user: bool | None = None
    extra: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        payload = dict(self.extra)
        payload.update(
            {
                "outcome": self.outcome,
                "component": self.component,
                "operation": self.operation,
                "reason": self.reason,
                "risk_level": self.risk_level,
                "resource": self.resource,
            }
        )
        if self.approval_grant_id is not None:
            payload["approval_grant_id"] = self.approval_grant_id
        if self.approved_by_user is not None:
            payload["approved_by_user"] = self.approved_by_user
        return payload

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> PolicyObservationRecord:
        known = {
            "outcome",
            "component",
            "operation",
            "reason",
            "risk_level",
            "resource",
            "approval_grant_id",
            "approved_by_user",
        }
        return cls(
            outcome=_optional_str(payload.get("outcome")),
            component=_optional_str(payload.get("component")),
            operation=_optional_str(payload.get("operation")),
            reason=_optional_str(payload.get("reason")),
            risk_level=_optional_str(payload.get("risk_level")),
            resource=_optional_str(payload.get("resource")),
            approval_grant_id=_optional_str(payload.get("approval_grant_id")),
            approved_by_user=_optional_bool(payload.get("approved_by_user")),
            extra={key: value for key, value in payload.items() if key not in known},
        )


@dataclass(frozen=True)
class ToolResultRecord:
    tool_call_id: str | None = None
    tool_name: str | None = None
    action_id: str | None = None
    ok: bool | None = None
    status: str | None = None
    error_code: str | None = None
    failure: dict[str, Any] | None = None
    extra: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        payload = dict(self.extra)
        payload.update(
            {
                "tool_call_id": self.tool_call_id,
                "tool_name": self.tool_name,
                "action_id": self.action_id,
                "ok": self.ok,
                "status": self.status,
                "error_code": self.error_code,
                "failure": self.failure,
            }
        )
        return payload

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> ToolResultRecord:
        known = {"tool_call_id", "tool_name", "action_id", "ok", "status", "error_code", "failure"}
        failure = payload.get("failure")
        return cls(
            tool_call_id=_optional_str(payload.get("tool_call_id")),
            tool_name=_optional_str(payload.get("tool_name")),
            action_id=_optional_str(payload.get("action_id")),
            ok=_optional_bool(payload.get("ok")),
            status=_optional_str(payload.get("status")),
            error_code=_optional_str(payload.get("error_code")),
            failure=dict(failure) if isinstance(failure, dict) else None,
            extra={key: value for key, value in payload.items() if key not in known},
        )


@dataclass(frozen=True)
class TaskOutcomeRecord:
    status: str
    error_code: str | None = None
    summary: str | None = None
    reason: str | None = None
    next_action: str | None = None
    retry_allowed: bool | None = None
    missing_evidence: list[str] = field(default_factory=list)
    extra: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        payload = dict(self.extra)
        payload.update(
            {
                "status": self.status,
                "error_code": self.error_code,
                "summary": self.summary,
                "reason": self.reason,
                "next_action": self.next_action,
                "retry_allowed": self.retry_allowed,
                "missing_evidence": list(self.missing_evidence),
            }
        )
        return payload

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> TaskOutcomeRecord:
        known = {
            "status",
            "error_code",
            "summary",
            "reason",
            "next_action",
            "retry_allowed",
            "missing_evidence",
        }
        return cls(
            status=str(payload.get("status") or "unknown"),
            error_code=_optional_str(payload.get("error_code")),
            summary=_optional_str(payload.get("summary")),
            reason=_optional_str(payload.get("reason")),
            next_action=_optional_str(payload.get("next_action")),
            retry_allowed=_optional_bool(payload.get("retry_allowed")),
            missing_evidence=[str(item) for item in payload.get("missing_evidence") or []],
            extra={key: value for key, value in payload.items() if key not in known},
        )


@dataclass
class EvidenceLedger:
    inspected_files: list[str] = field(default_factory=list)
    relevant_symbols: list[dict[str, Any]] = field(default_factory=list)
    search_results: list[dict[str, Any]] = field(default_factory=list)
    applied_changes: list[dict[str, Any]] = field(default_factory=list)
    command_results: list[dict[str, Any]] = field(default_factory=list)
    verification_results: list[dict[str, Any]] = field(default_factory=list)
    parsed_failures: list[dict[str, Any]] = field(default_factory=list)
    assumptions: list[str] = field(default_factory=list)
    missing_evidence: list[str] = field(default_factory=list)
    unresolved_failures: list[dict[str, Any]] = field(default_factory=list)
    external_changes: list[str] = field(default_factory=list)
    risks: list[dict[str, Any]] = field(default_factory=list)
    tool_results: list[dict[str, Any]] = field(default_factory=list)
    policy_observations: list[dict[str, Any]] = field(default_factory=list)
    sandbox_observations: list[dict[str, Any]] = field(default_factory=list)
    instruction_prompt_observations: list[dict[str, Any]] = field(default_factory=list)
    project_index_observations: list[dict[str, Any]] = field(default_factory=list)
    diff_observations: list[dict[str, Any]] = field(default_factory=list)
    edit_plans: list[dict[str, Any]] = field(default_factory=list)
    edit_results: list[dict[str, Any]] = field(default_factory=list)
    review_results: list[dict[str, Any]] = field(default_factory=list)
    failure_analyses: list[dict[str, Any]] = field(default_factory=list)
    repair_plans: list[dict[str, Any]] = field(default_factory=list)
    retrieval_results: list[dict[str, Any]] = field(default_factory=list)
    task_outcomes: list[dict[str, Any]] = field(default_factory=list)

    def add_unique_file(self, path: str) -> None:
        if path and path not in self.inspected_files:
            self.inspected_files.append(path)

    # --- Final Reviewer / Failure Analyzer query API (criterion-keyed) ---

    _EVIDENCE_KEY_TO_BUCKET: ClassVar[dict[str, str]] = {
        "inspected_files": "inspected_files",
        "relevant_symbols": "relevant_symbols",
        "search_results": "search_results",
        "applied_changes": "applied_changes",
        "command_results": "command_results",
        "verification_results": "verification_results",
        "parsed_failures": "parsed_failures",
        "missing_evidence": "missing_evidence",
        "unresolved_failures": "unresolved_failures",
        "external_changes": "external_changes",
        "risks": "risks",
        "tool_results": "tool_results",
        "policy_observations": "policy_observations",
        "sandbox_observations": "sandbox_observations",
        "instruction_prompt_observations": "instruction_prompt_observations",
        "project_index_observations": "project_index_observations",
        "diff_observations": "diff_observations",
        "edit_plans": "edit_plans",
        "edit_results": "edit_results",
        "review_results": "review_results",
        "failure_analyses": "failure_analyses",
        "repair_plans": "repair_plans",
        "retrieval_results": "retrieval_results",
        "task_outcomes": "task_outcomes",
    }

    def query_evidence(self, evidence_key: str) -> list[Any]:
        """Return the evidence records for a known evidence_key.

        Maps well-known keys (``inspected_files``, ``applied_changes``,
        ``verification_results`` ...) to their bucket. Unknown keys are
        resolved via ``getattr`` so callers can query any attribute.
        Returns an empty list when the bucket is absent or empty.
        """
        bucket_name = self._EVIDENCE_KEY_TO_BUCKET.get(evidence_key, evidence_key)
        value: Any = getattr(self, bucket_name, None)
        if value is None:
            return []
        if isinstance(value, list):
            return value
        return [value]

    def add_verification_result(
        self, record: VerificationEvidenceRecord | dict[str, Any]
    ) -> VerificationEvidenceRecord:
        typed = (
            record
            if isinstance(record, VerificationEvidenceRecord)
            else VerificationEvidenceRecord.from_dict(record)
        )
        self.verification_results.append(typed.to_dict())
        return typed

    def latest_verification_result(self) -> VerificationEvidenceRecord | None:
        if not self.verification_results:
            return None
        return VerificationEvidenceRecord.from_dict(self.verification_results[-1])

    def verification_records(self) -> list[VerificationEvidenceRecord]:
        return [
            VerificationEvidenceRecord.from_dict(item)
            for item in self.verification_results
        ]

    def add_sandbox_observation(
        self, record: SandboxObservationRecord | dict[str, Any]
    ) -> SandboxObservationRecord:
        typed = (
            record
            if isinstance(record, SandboxObservationRecord)
            else SandboxObservationRecord.from_dict(record)
        )
        payload = typed.to_dict()
        if payload not in self.sandbox_observations:
            self.sandbox_observations.append(payload)
        return typed

    def sandbox_records(self) -> list[SandboxObservationRecord]:
        return [
            SandboxObservationRecord.from_dict(item)
            for item in self.sandbox_observations
        ]

    def add_policy_observation(
        self, record: PolicyObservationRecord | dict[str, Any]
    ) -> PolicyObservationRecord:
        typed = (
            record
            if isinstance(record, PolicyObservationRecord)
            else PolicyObservationRecord.from_dict(record)
        )
        payload = typed.to_dict()
        if payload not in self.policy_observations:
            self.policy_observations.append(payload)
        return typed

    def policy_records(self) -> list[PolicyObservationRecord]:
        return [
            PolicyObservationRecord.from_dict(item)
            for item in self.policy_observations
        ]

    def add_tool_result(
        self, record: ToolResultRecord | dict[str, Any]
    ) -> ToolResultRecord:
        typed = (
            record
            if isinstance(record, ToolResultRecord)
            else ToolResultRecord.from_dict(record)
        )
        self.tool_results.append(typed.to_dict())
        return typed

    def tool_result_records(self) -> list[ToolResultRecord]:
        return [
            ToolResultRecord.from_dict(item)
            for item in self.tool_results
        ]

    def add_task_outcome(
        self, record: TaskOutcomeRecord | dict[str, Any]
    ) -> TaskOutcomeRecord:
        typed = (
            record
            if isinstance(record, TaskOutcomeRecord)
            else TaskOutcomeRecord.from_dict(record)
        )
        payload = typed.to_dict()
        if payload not in self.task_outcomes:
            self.task_outcomes.append(payload)
        return typed

    def task_outcome_records(self) -> list[TaskOutcomeRecord]:
        return [
            TaskOutcomeRecord.from_dict(item)
            for item in self.task_outcomes
        ]

    def evidence_for_criterion(self, criterion_id: str) -> list[dict[str, Any]]:
        """Return all evidence records tagged with ``criterion_id``.

        Walks every dict-bearing bucket and collects records whose
        ``criterion_id`` field matches. Buckets that hold non-dict items
        (``inspected_files`` holds strings, ``assumptions``/``missing_evidence``/
        ``external_changes`` hold strings) are skipped automatically.
        """
        matched: list[dict[str, Any]] = []
        for bucket_name in self._EVIDENCE_KEY_TO_BUCKET.values():
            bucket = getattr(self, bucket_name, None)
            if not isinstance(bucket, list):
                continue
            for record in bucket:
                if isinstance(record, dict) and record.get("criterion_id") == criterion_id:
                    matched.append(record)
        return matched

    def to_dict(self) -> dict[str, Any]:
        return {
            "inspected_files": self.inspected_files,
            "relevant_symbols": self.relevant_symbols,
            "search_results": self.search_results,
            "applied_changes": self.applied_changes,
            "command_results": self.command_results,
            "verification_results": self.verification_results,
            "parsed_failures": self.parsed_failures,
            "assumptions": self.assumptions,
            "missing_evidence": self.missing_evidence,
            "unresolved_failures": self.unresolved_failures,
            "external_changes": self.external_changes,
            "risks": self.risks,
            "tool_results": self.tool_results,
            "policy_observations": self.policy_observations,
            "sandbox_observations": self.sandbox_observations,
            "instruction_prompt_observations": self.instruction_prompt_observations,
            "project_index_observations": self.project_index_observations,
            "diff_observations": self.diff_observations,
            "edit_plans": self.edit_plans,
            "edit_results": self.edit_results,
            "review_results": self.review_results,
            "failure_analyses": self.failure_analyses,
            "repair_plans": self.repair_plans,
            "retrieval_results": self.retrieval_results,
            "task_outcomes": self.task_outcomes,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> EvidenceLedger:
        return cls(
            inspected_files=list(payload.get("inspected_files") or []),
            relevant_symbols=list(payload.get("relevant_symbols") or []),
            search_results=list(payload.get("search_results") or []),
            applied_changes=list(payload.get("applied_changes") or []),
            command_results=list(payload.get("command_results") or []),
            verification_results=list(payload.get("verification_results") or []),
            parsed_failures=list(payload.get("parsed_failures") or []),
            assumptions=list(payload.get("assumptions") or []),
            missing_evidence=list(payload.get("missing_evidence") or []),
            unresolved_failures=list(payload.get("unresolved_failures") or []),
            external_changes=list(payload.get("external_changes") or []),
            risks=list(payload.get("risks") or []),
            tool_results=list(payload.get("tool_results") or []),
            policy_observations=list(payload.get("policy_observations") or []),
            sandbox_observations=list(payload.get("sandbox_observations") or []),
            instruction_prompt_observations=list(payload.get("instruction_prompt_observations") or []),
            project_index_observations=list(payload.get("project_index_observations") or []),
            diff_observations=list(payload.get("diff_observations") or []),
            edit_plans=list(payload.get("edit_plans") or []),
            edit_results=list(payload.get("edit_results") or []),
            review_results=list(payload.get("review_results") or []),
            failure_analyses=list(payload.get("failure_analyses") or []),
            repair_plans=list(payload.get("repair_plans") or []),
            retrieval_results=list(payload.get("retrieval_results") or []),
            task_outcomes=list(payload.get("task_outcomes") or []),
        )


def _optional_str(value: Any) -> str | None:
    return str(value) if value is not None else None


def _optional_bool(value: Any) -> bool | None:
    if isinstance(value, bool):
        return value
    if value is None:
        return None
    if isinstance(value, str):
        normalized = value.strip().lower()
        if normalized in {"true", "1", "yes"}:
            return True
        if normalized in {"false", "0", "no"}:
            return False
    return None


@dataclass
class ExecutionBudget:
    max_model_turns: int = 20
    max_tool_calls: int = 80
    max_command_runs: int = 20
    max_mutation_transactions: int = 20
    max_repair_iterations: int = 3
    max_changed_files: int = 30
    max_wall_time_seconds: int = 1800
    max_repeated_failures: int = 2
    max_context_growth: int = 128000
    model_turns: int = 0
    tool_calls: int = 0
    command_runs: int = 0
    mutation_transactions: int = 0
    repair_iterations: int = 0
    changed_files: int = 0
    context_growth: int = 0
    repeated_failures: dict[str, int] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return self.__dict__.copy()

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> ExecutionBudget:
        return cls(**payload)


@dataclass(frozen=True)
class AuthorizationDecision:
    allowed: bool
    action: AgentAction | None = None
    error_code: str | None = None
    reason: str = ""
    risk_decision: RiskDecisionKind = RiskDecisionKind.CONTINUE

    def to_dict(self) -> dict[str, Any]:
        return {
            "allowed": self.allowed,
            "action": self.action.to_dict() if self.action else None,
            "error_code": self.error_code,
            "reason": self.reason,
            "risk_decision": self.risk_decision.value,
        }


@dataclass(frozen=True)
class ReplanDecision:
    decision: ReplanDecisionKind
    reason: str
    next_action: ActionKind | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "decision": self.decision.value,
            "reason": self.reason,
            "next_action": self.next_action.value if self.next_action else None,
        }


@dataclass(frozen=True)
class RiskEscalation:
    decision: RiskDecisionKind
    risk_level: RiskLevel
    reasons: list[str]

    def to_dict(self) -> dict[str, Any]:
        return {
            "decision": self.decision.value,
            "risk_level": self.risk_level.value,
            "reasons": self.reasons,
        }


@dataclass
class FinalReport:
    user_goal: str
    status: TaskStatus
    files_changed: list[str]
    agent_changes: list[dict[str, Any]]
    command_side_effects: list[dict[str, Any]]
    verification_summary: dict[str, Any]
    unresolved_issues: list[Any]
    risks: list[Any]
    rollback_status: dict[str, Any]
    policy_approval_summary: dict[str, Any]
    artifacts: list[str]
    next_steps: list[str]
    sandbox_isolation_summary: dict[str, Any] = field(default_factory=dict)
    execution_trace_summary: dict[str, Any] = field(default_factory=dict)
    model_usage_summary: dict[str, Any] = field(default_factory=dict)
    context_usage_diagnostic: dict[str, Any] = field(default_factory=dict)
    instruction_prompt_summary: dict[str, Any] = field(default_factory=dict)
    component_health_summary: dict[str, Any] = field(default_factory=dict)
    shutdown_summary: dict[str, Any] = field(default_factory=dict)
    recovery_summary: dict[str, Any] = field(default_factory=dict)
    lifecycle_summary: dict[str, Any] = field(default_factory=dict)
    review_summary: dict[str, Any] = field(default_factory=dict)
    failure_repair_summary: dict[str, Any] = field(default_factory=dict)
    contract_satisfaction: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "user_goal": self.user_goal,
            "status": self.status.value,
            "files_changed": self.files_changed,
            "agent_changes": self.agent_changes,
            "command_side_effects": self.command_side_effects,
            "verification_summary": self.verification_summary,
            "unresolved_issues": self.unresolved_issues,
            "risks": self.risks,
            "rollback_status": self.rollback_status,
            "policy_approval_summary": self.policy_approval_summary,
            "sandbox_isolation_summary": self.sandbox_isolation_summary,
            "execution_trace_summary": self.execution_trace_summary,
            "model_usage_summary": self.model_usage_summary,
            "context_usage_diagnostic": self.context_usage_diagnostic,
            "instruction_prompt_summary": self.instruction_prompt_summary,
            "component_health_summary": self.component_health_summary,
            "shutdown_summary": self.shutdown_summary,
            "recovery_summary": self.recovery_summary,
            "lifecycle_summary": self.lifecycle_summary,
            "review_summary": self.review_summary,
            "failure_repair_summary": self.failure_repair_summary,
            "contract_satisfaction": self.contract_satisfaction,
            "artifacts": self.artifacts,
            "next_steps": self.next_steps,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> FinalReport:
        return cls(
            user_goal=str(payload["user_goal"]),
            status=TaskStatus(payload["status"]),
            files_changed=list(payload.get("files_changed") or []),
            agent_changes=list(payload.get("agent_changes") or []),
            command_side_effects=list(payload.get("command_side_effects") or []),
            verification_summary=dict(payload.get("verification_summary") or {}),
            unresolved_issues=list(payload.get("unresolved_issues") or []),
            risks=list(payload.get("risks") or []),
            rollback_status=dict(payload.get("rollback_status") or {}),
            policy_approval_summary=dict(payload.get("policy_approval_summary") or {}),
            sandbox_isolation_summary=dict(payload.get("sandbox_isolation_summary") or {}),
            execution_trace_summary=dict(payload.get("execution_trace_summary") or {}),
            model_usage_summary=dict(payload.get("model_usage_summary") or {}),
            context_usage_diagnostic=dict(payload.get("context_usage_diagnostic") or {}),
            instruction_prompt_summary=dict(payload.get("instruction_prompt_summary") or {}),
            component_health_summary=dict(payload.get("component_health_summary") or {}),
            shutdown_summary=dict(payload.get("shutdown_summary") or {}),
            recovery_summary=dict(payload.get("recovery_summary") or {}),
            lifecycle_summary=dict(payload.get("lifecycle_summary") or {}),
            review_summary=dict(payload.get("review_summary") or {}),
            failure_repair_summary=dict(payload.get("failure_repair_summary") or {}),
            contract_satisfaction=dict(payload.get("contract_satisfaction") or {}),
            artifacts=list(payload.get("artifacts") or []),
            next_steps=list(payload.get("next_steps") or []),
        )


def _now() -> str:
    return datetime.now(UTC).isoformat()
