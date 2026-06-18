from __future__ import annotations

from dataclasses import dataclass, field
from datetime import UTC, datetime
from enum import Enum
from typing import Any
from uuid import uuid4


class TaskStatus(str, Enum):
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


class ActionKind(str, Enum):
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


class ActionStatus(str, Enum):
    PROPOSED = "proposed"
    ALLOWED = "allowed"
    RUNNING = "running"
    SUCCEEDED = "succeeded"
    FAILED = "failed"
    BLOCKED = "blocked"


class RiskLevel(str, Enum):
    LOW = "low"
    MEDIUM = "medium"
    HIGH = "high"
    CRITICAL = "critical"


class RiskDecisionKind(str, Enum):
    CONTINUE = "continue"
    REQUIRE_REVIEW = "require_review"
    ASK_USER = "ask_user"
    DENY_ACTION = "deny_action"
    ABORT = "abort"


class ReplanDecisionKind(str, Enum):
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
    def from_dict(cls, payload: dict[str, Any]) -> "CompletionCriteria":
        return cls(**payload)


@dataclass
class TaskState:
    task_id: str
    session_id: str
    user_goal: str
    normalized_goal: str
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

    def touch(self) -> None:
        self.updated_at = _now()

    def to_dict(self) -> dict[str, Any]:
        return {
            "task_id": self.task_id,
            "session_id": self.session_id,
            "user_goal": self.user_goal,
            "normalized_goal": self.normalized_goal,
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
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "TaskState":
        return cls(
            task_id=str(payload["task_id"]),
            session_id=str(payload["session_id"]),
            user_goal=str(payload["user_goal"]),
            normalized_goal=str(payload["normalized_goal"]),
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
    def from_dict(cls, payload: dict[str, Any]) -> "TaskPhase":
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
    def from_dict(cls, payload: dict[str, Any]) -> "TaskPlan":
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
    def from_dict(cls, payload: dict[str, Any]) -> "AgentAction":
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

    def add_unique_file(self, path: str) -> None:
        if path and path not in self.inspected_files:
            self.inspected_files.append(path)

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
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "EvidenceLedger":
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
        )


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
    def from_dict(cls, payload: dict[str, Any]) -> "ExecutionBudget":
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
    instruction_prompt_summary: dict[str, Any] = field(default_factory=dict)
    runtime_health_summary: dict[str, Any] = field(default_factory=dict)
    shutdown_summary: dict[str, Any] = field(default_factory=dict)
    recovery_summary: dict[str, Any] = field(default_factory=dict)
    lifecycle_summary: dict[str, Any] = field(default_factory=dict)

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
            "instruction_prompt_summary": self.instruction_prompt_summary,
            "runtime_health_summary": self.runtime_health_summary,
            "shutdown_summary": self.shutdown_summary,
            "recovery_summary": self.recovery_summary,
            "lifecycle_summary": self.lifecycle_summary,
            "artifacts": self.artifacts,
            "next_steps": self.next_steps,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "FinalReport":
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
            instruction_prompt_summary=dict(payload.get("instruction_prompt_summary") or {}),
            runtime_health_summary=dict(payload.get("runtime_health_summary") or {}),
            shutdown_summary=dict(payload.get("shutdown_summary") or {}),
            recovery_summary=dict(payload.get("recovery_summary") or {}),
            lifecycle_summary=dict(payload.get("lifecycle_summary") or {}),
            artifacts=list(payload.get("artifacts") or []),
            next_steps=list(payload.get("next_steps") or []),
        )


def _now() -> str:
    return datetime.now(UTC).isoformat()
