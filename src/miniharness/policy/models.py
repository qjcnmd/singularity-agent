from __future__ import annotations

import fnmatch
import json
import os
from dataclasses import dataclass, field
from datetime import UTC, datetime
from enum import Enum
from pathlib import Path
from typing import Any
from uuid import uuid4


class RuntimeName(str, Enum):
    TOOL = "tool"
    MUTATION = "mutation"
    COMMAND = "command"
    VERIFICATION = "verification"
    PLANNER = "planner"
    WORKSPACE_STATE = "workspace_state"
    SYSTEM = "system"


class OperationKind(str, Enum):
    READ_FILE = "read_file"
    LIST_DIRECTORY = "list_directory"
    SEARCH = "search"
    MUTATE_FILE = "mutate_file"
    CREATE_FILE = "create_file"
    DELETE_FILE = "delete_file"
    ROLLBACK = "rollback"
    EXECUTE_COMMAND = "execute_command"
    EXECUTE_PROJECT_CODE = "execute_project_code"
    PACKAGE_INSTALL = "package_install"
    NETWORK_ACCESS = "network_access"
    START_LONG_PROCESS = "start_long_process"
    KILL_PROCESS = "kill_process"
    READ_ENV = "read_env"
    CHANGE_CONFIG = "change_config"
    VERIFICATION = "verification"


class Capability(str, Enum):
    READ_WORKSPACE = "READ_WORKSPACE"
    READ_OUTSIDE_WORKSPACE = "READ_OUTSIDE_WORKSPACE"
    READ_SECRET = "READ_SECRET"
    LIST_DIRECTORY = "LIST_DIRECTORY"
    MUTATE_WORKSPACE = "MUTATE_WORKSPACE"
    CREATE_FILE = "CREATE_FILE"
    DELETE_FILE = "DELETE_FILE"
    MOVE_FILE = "MOVE_FILE"
    ROLLBACK_MUTATION = "ROLLBACK_MUTATION"
    EXECUTE_COMMAND = "EXECUTE_COMMAND"
    EXECUTE_PROJECT_CODE = "EXECUTE_PROJECT_CODE"
    EXECUTE_GENERATED_CODE = "EXECUTE_GENERATED_CODE"
    NETWORK_ACCESS = "NETWORK_ACCESS"
    PACKAGE_INSTALL = "PACKAGE_INSTALL"
    PACKAGE_SCRIPT = "PACKAGE_SCRIPT"
    START_LONG_PROCESS = "START_LONG_PROCESS"
    KILL_PROCESS = "KILL_PROCESS"
    READ_ENV = "READ_ENV"
    WRITE_ENV = "WRITE_ENV"
    CHANGE_AGENT_CONFIG = "CHANGE_AGENT_CONFIG"


class RiskTag(str, Enum):
    WORKSPACE_READ = "WORKSPACE_READ"
    OUTSIDE_WORKSPACE = "OUTSIDE_WORKSPACE"
    SECRET_ACCESS = "SECRET_ACCESS"
    MUTATES_FILES = "MUTATES_FILES"
    MUTATES_CONFIG = "MUTATES_CONFIG"
    MUTATES_LOCKFILE = "MUTATES_LOCKFILE"
    DESTRUCTIVE = "DESTRUCTIVE"
    IRREVERSIBLE = "IRREVERSIBLE"
    EXECUTES_CODE = "EXECUTES_CODE"
    EXECUTES_PROJECT_CODE = "EXECUTES_PROJECT_CODE"
    EXECUTES_GENERATED_CODE = "EXECUTES_GENERATED_CODE"
    SHELL_EXPANSION = "SHELL_EXPANSION"
    NETWORK = "NETWORK"
    PACKAGE_MANAGER = "PACKAGE_MANAGER"
    SUPPLY_CHAIN = "SUPPLY_CHAIN"
    LONG_RUNNING = "LONG_RUNNING"
    RESOURCE_HEAVY = "RESOURCE_HEAVY"
    PERSISTENT_SIDE_EFFECT = "PERSISTENT_SIDE_EFFECT"
    SECRETS_EXFILTRATION = "SECRETS_EXFILTRATION"


class RiskLevel(str, Enum):
    NONE = "none"
    LOW = "low"
    MEDIUM = "medium"
    HIGH = "high"
    CRITICAL = "critical"


class DecisionOutcome(str, Enum):
    ALLOW = "allow"
    DENY = "deny"
    REQUIRE_REVIEW = "require_review"
    ASK_USER = "ask_user"
    ESCALATE = "escalate"
    SANDBOX_REQUIRED = "sandbox_required"


@dataclass(frozen=True)
class PolicySubject:
    subject_type: str
    name: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return {"subject_type": self.subject_type, "name": self.name}


@dataclass(frozen=True)
class ResourceRef:
    resource_type: str
    identifier: str
    normalized_identifier: str | None = None
    workspace_relative: bool = False
    sensitive: bool = False
    metadata: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "resource_type": self.resource_type,
            "identifier": self.identifier,
            "normalized_identifier": self.normalized_identifier,
            "workspace_relative": self.workspace_relative,
            "sensitive": self.sensitive,
            "metadata": self.metadata,
        }


@dataclass(frozen=True)
class PolicyConstraints:
    filesystem_mode: str = "none"
    network_allowed: bool = False
    max_duration_seconds: int | None = None
    max_output_chars: int | None = None
    allowed_paths: list[str] = field(default_factory=list)
    denied_paths: list[str] = field(default_factory=list)
    allowed_hosts: list[str] = field(default_factory=list)
    env_redaction: bool = True
    sandbox_required: bool = False
    hard_isolation_required: bool = False

    def to_dict(self) -> dict[str, Any]:
        return {
            "filesystem_mode": self.filesystem_mode,
            "network_allowed": self.network_allowed,
            "max_duration_seconds": self.max_duration_seconds,
            "max_output_chars": self.max_output_chars,
            "allowed_paths": self.allowed_paths,
            "denied_paths": self.denied_paths,
            "allowed_hosts": self.allowed_hosts,
            "env_redaction": self.env_redaction,
            "sandbox_required": self.sandbox_required,
            "hard_isolation_required": self.hard_isolation_required,
        }


@dataclass(frozen=True)
class PolicyRequest:
    session_id: str
    task_id: str
    phase_id: str
    action_id: str
    runtime: RuntimeName | str
    operation: OperationKind | str
    capability: Capability | str
    subject: PolicySubject
    resource: ResourceRef
    reason: str
    request_id: str = field(default_factory=lambda: f"policy_req_{uuid4().hex[:12]}")
    proposed_by_model: bool = False
    risk_tags: list[RiskTag | str] = field(default_factory=list)
    metadata: dict[str, Any] = field(default_factory=dict)
    evidence_refs: list[str] = field(default_factory=list)
    reversible: bool = True
    requires_network: bool = False
    touches_workspace: bool = False
    touches_secrets: bool = False
    destructive: bool = False
    long_running: bool = False
    interactive: bool = False
    workspace_root: str | None = None

    def __post_init__(self) -> None:
        object.__setattr__(self, "runtime", _enum(RuntimeName, self.runtime))
        object.__setattr__(self, "operation", _enum(OperationKind, self.operation))
        object.__setattr__(self, "capability", _enum(Capability, self.capability))
        object.__setattr__(
            self,
            "risk_tags",
            [_enum(RiskTag, tag) if str(tag) in RiskTag._value2member_map_ or str(tag) in RiskTag.__members__ else str(tag) for tag in self.risk_tags],
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "request_id": self.request_id,
            "session_id": self.session_id,
            "task_id": self.task_id,
            "phase_id": self.phase_id,
            "action_id": self.action_id,
            "runtime": self.runtime.value,
            "operation": self.operation.value,
            "capability": self.capability.value,
            "subject": self.subject.to_dict(),
            "resource": self.resource.to_dict(),
            "reason": self.reason,
            "proposed_by_model": self.proposed_by_model,
            "risk_tags": [_value(tag) for tag in self.risk_tags],
            "metadata": self.metadata,
            "evidence_refs": self.evidence_refs,
            "reversible": self.reversible,
            "requires_network": self.requires_network,
            "touches_workspace": self.touches_workspace,
            "touches_secrets": self.touches_secrets,
            "destructive": self.destructive,
            "long_running": self.long_running,
            "interactive": self.interactive,
            "workspace_root": self.workspace_root,
        }


@dataclass(frozen=True)
class ApprovalScope:
    capabilities: list[Capability | str] = field(default_factory=list)
    path_globs: list[str] = field(default_factory=list)
    command_patterns: list[str] = field(default_factory=list)
    network_hosts: list[str] = field(default_factory=list)
    max_duration_seconds: int | None = None
    max_files: int | None = None
    session_only: bool = True
    single_use: bool = True

    def __post_init__(self) -> None:
        object.__setattr__(
            self,
            "capabilities",
            [_enum(Capability, capability) for capability in self.capabilities],
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "capabilities": [capability.value for capability in self.capabilities],
            "path_globs": self.path_globs,
            "command_patterns": self.command_patterns,
            "network_hosts": self.network_hosts,
            "max_duration_seconds": self.max_duration_seconds,
            "max_files": self.max_files,
            "session_only": self.session_only,
            "single_use": self.single_use,
        }


@dataclass(frozen=True)
class ApprovalRequirement:
    message: str
    scope: ApprovalScope
    review_kind: str = "generic"
    details: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "message": self.message,
            "scope": self.scope.to_dict(),
            "review_kind": self.review_kind,
            "details": self.details,
        }


@dataclass
class ApprovalGrant:
    decision_id: str
    request_id: str
    approved_by: str
    scope: ApprovalScope
    approved_at: str = field(default_factory=lambda: datetime.now(UTC).isoformat())
    grant_id: str = field(default_factory=lambda: f"grant_{uuid4().hex[:12]}")
    expires_at: str | None = None
    single_use: bool = True
    consumed: bool = False
    reason: str = ""

    def matches(self, request: PolicyRequest, *, workspace_root: Path | str | None) -> bool:
        if self.consumed:
            return False
        if self.expires_at and datetime.fromisoformat(self.expires_at) < datetime.now(UTC):
            return False
        if request.capability not in self.scope.capabilities:
            return False
        if request.resource.resource_type == "command":
            return _matches_any(request.resource.identifier, self.scope.command_patterns)
        if request.resource.resource_type == "network":
            host = str(request.resource.metadata.get("host") or request.resource.identifier)
            return _matches_any(host, self.scope.network_hosts)
        if request.resource.resource_type in {"file", "directory", "workspace", "config"}:
            normalized = _resource_path_for_match(request, workspace_root)
            return _matches_any(normalized, self.scope.path_globs)
        return True

    def consume(self) -> None:
        if self.single_use or self.scope.single_use:
            self.consumed = True

    def to_dict(self) -> dict[str, Any]:
        return {
            "grant_id": self.grant_id,
            "decision_id": self.decision_id,
            "request_id": self.request_id,
            "approved_by": self.approved_by,
            "approved_at": self.approved_at,
            "scope": self.scope.to_dict(),
            "expires_at": self.expires_at,
            "single_use": self.single_use,
            "consumed": self.consumed,
            "reason": self.reason,
        }


@dataclass(frozen=True)
class PolicyDecision:
    request_id: str
    outcome: DecisionOutcome | str
    reason: str
    risk_level: RiskLevel | str = RiskLevel.NONE
    risk_tags: list[RiskTag | str] = field(default_factory=list)
    user_message: str = ""
    constraints: PolicyConstraints = field(default_factory=PolicyConstraints)
    required_approval: ApprovalRequirement | None = None
    rule_ids: list[str] = field(default_factory=list)
    audit_severity: str = "info"
    context_summary: str = ""
    decision_id: str = field(default_factory=lambda: f"policy_dec_{uuid4().hex[:12]}")
    approval_grant_id: str | None = None

    def __post_init__(self) -> None:
        object.__setattr__(self, "outcome", _enum(DecisionOutcome, self.outcome))
        object.__setattr__(self, "risk_level", _enum(RiskLevel, self.risk_level))
        object.__setattr__(
            self,
            "risk_tags",
            [_enum(RiskTag, tag) if str(tag) in RiskTag._value2member_map_ or str(tag) in RiskTag.__members__ else str(tag) for tag in self.risk_tags],
        )

    @classmethod
    def review(
        cls,
        *,
        request: PolicyRequest,
        reason: str,
        message: str,
        risk_level: RiskLevel = RiskLevel.MEDIUM,
        risk_tags: list[RiskTag | str] | None = None,
        review_kind: str = "generic",
    ) -> "PolicyDecision":
        return cls(
            request_id=request.request_id,
            outcome=DecisionOutcome.REQUIRE_REVIEW,
            risk_level=risk_level,
            risk_tags=risk_tags or [],
            reason=reason,
            user_message=message,
            required_approval=ApprovalRequirement(
                message=message,
                scope=approval_scope_for_request(request),
                review_kind=review_kind,
                details={
                    "runtime": request.runtime.value,
                    "operation": request.operation.value,
                    "resource": request.resource.identifier,
                },
            ),
            context_summary=policy_context_summary(request, DecisionOutcome.REQUIRE_REVIEW, reason),
        )

    def model_copy_with(self, **updates: Any) -> "PolicyDecision":
        payload = {
            "request_id": self.request_id,
            "outcome": self.outcome,
            "risk_level": self.risk_level,
            "risk_tags": self.risk_tags,
            "reason": self.reason,
            "user_message": self.user_message,
            "constraints": self.constraints,
            "required_approval": self.required_approval,
            "rule_ids": self.rule_ids,
            "audit_severity": self.audit_severity,
            "context_summary": self.context_summary,
            "decision_id": self.decision_id,
            "approval_grant_id": self.approval_grant_id,
        }
        payload.update(updates)
        return PolicyDecision(**payload)

    def to_dict(self) -> dict[str, Any]:
        return {
            "decision_id": self.decision_id,
            "request_id": self.request_id,
            "outcome": self.outcome.value,
            "risk_level": self.risk_level.value,
            "risk_tags": [_value(tag) for tag in self.risk_tags],
            "reason": self.reason,
            "user_message": self.user_message,
            "constraints": self.constraints.to_dict(),
            "required_approval": (
                self.required_approval.to_dict() if self.required_approval else None
            ),
            "rule_ids": self.rule_ids,
            "audit_severity": self.audit_severity,
            "context_summary": self.context_summary,
            "approval_grant_id": self.approval_grant_id,
        }


@dataclass(frozen=True)
class PolicyAuditEntry:
    timestamp: str
    session_id: str
    task_id: str
    phase_id: str
    action_id: str
    request_id: str
    decision_id: str
    runtime: RuntimeName | str
    operation: OperationKind | str
    capability: Capability | str
    resource_summary: str
    normalized_input_hash: str
    risk_level: RiskLevel | str
    risk_tags: list[RiskTag | str]
    outcome: DecisionOutcome | str
    rule_ids: list[str]
    reason: str
    approval_required: bool
    approval_grant_id: str | None = None
    approved_by_user: bool = False
    user_decision: str | None = None
    constraints: dict[str, Any] = field(default_factory=dict)
    execution_result_ref: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "timestamp": self.timestamp,
            "session_id": self.session_id,
            "task_id": self.task_id,
            "phase_id": self.phase_id,
            "action_id": self.action_id,
            "request_id": self.request_id,
            "decision_id": self.decision_id,
            "runtime": _value(self.runtime),
            "operation": _value(self.operation),
            "capability": _value(self.capability),
            "resource_summary": self.resource_summary,
            "normalized_input_hash": self.normalized_input_hash,
            "risk_level": _value(self.risk_level),
            "risk_tags": [_value(tag) for tag in self.risk_tags],
            "outcome": _value(self.outcome),
            "rule_ids": self.rule_ids,
            "reason": self.reason,
            "approval_required": self.approval_required,
            "approval_grant_id": self.approval_grant_id,
            "approved_by_user": self.approved_by_user,
            "user_decision": self.user_decision,
            "constraints": self.constraints,
            "execution_result_ref": self.execution_result_ref,
        }


def approval_scope_for_request(request: PolicyRequest) -> ApprovalScope:
    path_globs: list[str] = []
    command_patterns: list[str] = []
    network_hosts: list[str] = []
    if request.resource.resource_type in {"file", "directory", "workspace", "config"}:
        path_globs.append(request.resource.normalized_identifier or request.resource.identifier)
    elif request.resource.resource_type == "command":
        command_patterns.append(request.resource.identifier)
    elif request.resource.resource_type == "network":
        network_hosts.append(
            str(request.resource.metadata.get("host") or request.resource.identifier)
        )
    return ApprovalScope(
        capabilities=[request.capability],
        path_globs=path_globs,
        command_patterns=command_patterns,
        network_hosts=network_hosts,
        max_duration_seconds=request.metadata.get("timeout"),
        session_only=True,
        single_use=True,
    )


def policy_context_summary(
    request: PolicyRequest, outcome: DecisionOutcome, reason: str
) -> str:
    runtime = request.runtime.value.replace("_", " ").title().replace(" ", "")
    return f"[policy] {runtime} {outcome.value.replace('_', ' ')}: {reason}"


def _enum(enum_type: type[Enum], value: Enum | str) -> Enum:
    if isinstance(value, enum_type):
        return value
    text = str(value)
    if text in enum_type.__members__:
        return enum_type[text]
    return enum_type(text)


def _value(value: Any) -> Any:
    return value.value if isinstance(value, Enum) else value


def _matches_any(value: str, patterns: list[str]) -> bool:
    return bool(patterns) and any(fnmatch.fnmatchcase(value, pattern) for pattern in patterns)


def _resource_path_for_match(
    request: PolicyRequest, workspace_root: Path | str | None
) -> str:
    identifier = request.resource.normalized_identifier or request.resource.identifier
    if workspace_root is None:
        return identifier
    root = Path(workspace_root).resolve(strict=False)
    raw = Path(identifier)
    candidate = raw if raw.is_absolute() else root / raw
    try:
        resolved = candidate.resolve(strict=False)
        return os.path.normcase(os.path.normpath(str(resolved)))
    except OSError:
        return identifier


def stable_hash(payload: Any) -> str:
    import hashlib

    text = json.dumps(payload, ensure_ascii=False, sort_keys=True, default=str)
    return hashlib.sha256(text.encode("utf-8")).hexdigest()
