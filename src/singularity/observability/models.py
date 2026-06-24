from __future__ import annotations

import json
from dataclasses import dataclass, field
from datetime import UTC, datetime
from enum import Enum
from pathlib import Path
from typing import Any, TypeVar

from singularity.observability.exceptions import TraceSerializationError

_EnumT = TypeVar("_EnumT", bound=Enum)


class TraceSeverity(str, Enum):
    DEBUG = "debug"
    INFO = "info"
    WARNING = "warning"
    ERROR = "error"
    CRITICAL = "critical"


class TraceEventType(str, Enum):
    TASK_STARTED = "task.started"
    TASK_COMPLETED = "task.completed"
    TASK_FAILED = "task.failed"
    PHASE_STARTED = "phase.started"
    PHASE_COMPLETED = "phase.completed"
    ACTION_PROPOSED = "action.proposed"
    ACTION_STARTED = "action.started"
    ACTION_COMPLETED = "action.completed"
    ACTION_FAILED = "action.failed"
    PLANNER_REPLAN_TRIGGERED = "planner.replan_triggered"
    PLANNER_COMPLETION_ASSESSED = "planner.completion_assessed"
    MODEL_REQUEST_CREATED = "model.request.created"
    MODEL_RESPONSE_RECEIVED = "model.response.received"
    MODEL_REQUEST_FAILED = "model.request.failed"
    MODEL_TOOL_CALL_PROPOSED = "model.tool_call.proposed"
    MODEL_OUTPUT_REJECTED = "model.output.rejected"
    TOOL_PROTOCOL_BATCH_CREATED = "tool_protocol.batch_created"
    TOOL_PROTOCOL_CALL_VALIDATED = "tool_protocol.call_validated"
    TOOL_PROTOCOL_CALL_REJECTED = "tool_protocol.call_rejected"
    TOOL_PROTOCOL_PLAN_BUILT = "tool_protocol.plan_built"
    TOOL_PROTOCOL_CALL_SCHEDULED = "tool_protocol.call_scheduled"
    TOOL_PROTOCOL_CALL_STARTED = "tool_protocol.call_started"
    TOOL_PROTOCOL_CALL_COMPLETED = "tool_protocol.call_completed"
    TOOL_PROTOCOL_PARALLEL_GROUP_STARTED = "tool_protocol.parallel_group_started"
    TOOL_PROTOCOL_PARALLEL_GROUP_COMPLETED = "tool_protocol.parallel_group_completed"
    TOOL_PROTOCOL_RESULT_BOUND = "tool_protocol.result_bound"
    TOOL_PROTOCOL_SYNTHETIC_RESULT_CREATED = "tool_protocol.synthetic_result_created"
    TOOL_PROTOCOL_REPLAY_DETECTED = "tool_protocol.replay_detected"
    TOOL_PROTOCOL_RECOVERY_STARTED = "tool_protocol.recovery_started"
    TOOL_PROTOCOL_RECOVERY_COMPLETED = "tool_protocol.recovery_completed"
    TOOL_VALIDATION_STARTED = "tool.validation.started"
    TOOL_VALIDATION_FAILED = "tool.validation.failed"
    TOOL_DISPATCH_STARTED = "tool.dispatch.started"
    TOOL_DISPATCH_COMPLETED = "tool.dispatch.completed"
    TOOL_DISPATCH_FAILED = "tool.dispatch.failed"
    PLUGIN_DISCOVERED = "plugin.discovered"
    PLUGIN_CHECK_FAILED = "plugin.check_failed"
    PLUGIN_ENABLED = "plugin.enabled"
    PLUGIN_DISABLED = "plugin.disabled"
    PLUGIN_LOAD_STARTED = "plugin.load_started"
    PLUGIN_LOAD_COMPLETED = "plugin.load_completed"
    PLUGIN_LOAD_FAILED = "plugin.load_failed"
    PLUGIN_ACTIVATED = "plugin.activated"
    PLUGIN_TOOL_REGISTERED = "plugin.tool_registered"
    PLUGIN_EVENT = "plugin.event"
    POLICY_REQUESTED = "policy.requested"
    POLICY_DECIDED = "policy.decided"
    POLICY_BLOCKED = "policy.blocked"
    APPROVAL_REQUESTED = "approval.requested"
    APPROVAL_GRANTED = "approval.granted"
    APPROVAL_DENIED = "approval.denied"
    USER_DECISION_RECORDED = "user_decision.recorded"
    CLARIFICATION_REQUESTED = "clarification.requested"
    CLARIFICATION_ANSWERED = "clarification.answered"
    CONTROL_COMMAND_RECEIVED = "control_command.received"
    COMMAND_REQUESTED = "command.requested"
    COMMAND_STARTED = "command.started"
    COMMAND_OUTPUT_CHUNK = "command.output_chunk"
    COMMAND_COMPLETED = "command.completed"
    COMMAND_FAILED = "command.failed"
    COMMAND_TIMEOUT = "command.timeout"
    COMMAND_KILLED = "command.killed"
    SANDBOX_REQUESTED = "sandbox.requested"
    SANDBOX_PREPARED = "sandbox.prepared"
    SANDBOX_CAPABILITY_FAILED = "sandbox.capability_failed"
    SANDBOX_STARTED = "sandbox.started"
    SANDBOX_COMPLETED = "sandbox.completed"
    SANDBOX_VIOLATION = "sandbox.violation"
    SANDBOX_CLEANED = "sandbox.cleaned"
    MUTATION_PROPOSED = "mutation.proposed"
    PATCH_PROPOSED = "patch.proposed"
    MUTATION_TRANSACTION_STARTED = "mutation.transaction_started"
    MUTATION_APPLIED = "mutation.applied"
    MUTATION_FAILED = "mutation.failed"
    MUTATION_ROLLBACK_STARTED = "mutation.rollback_started"
    MUTATION_ROLLBACK_COMPLETED = "mutation.rollback_completed"
    EDIT_PLAN_CREATED = "edit.plan_created"
    EDIT_PATCH_VALIDATED = "edit.patch_validated"
    EDIT_APPLIED = "edit.applied"
    EDIT_REPAIR_ATTEMPTED = "edit.repair_attempted"
    EDIT_FAILED = "edit.failed"
    REVIEW_STARTED = "review.started"
    REVIEW_FINDING = "review.finding"
    REVIEW_DECISION = "review.decision"
    REVIEW_COMPLETED = "review.completed"
    VERIFICATION_PLAN_CREATED = "verification.plan_created"
    VERIFICATION_CHECK_STARTED = "verification.check_started"
    VERIFICATION_CHECK_COMPLETED = "verification.check_completed"
    VERIFICATION_FAILED = "verification.failed"
    VERIFICATION_EVIDENCE_RECORDED = "verification.evidence_recorded"
    REPAIR_HINT_CREATED = "repair.hint_created"
    CONTEXT_SNAPSHOT_CREATED = "context.snapshot_created"
    CONTEXT_COMPACTED = "context.compacted"
    CONTEXT_OBSERVATION_ADDED = "context.observation_added"
    CONTEXT_RENDERED_FOR_MODEL = "context.rendered_for_model"
    CONTEXT_ITEM_ADDED = "context.item_added"
    CONTEXT_ITEM_REDACTED = "context.item_redacted"
    CONTEXT_ITEM_PINNED = "context.item_pinned"
    CONTEXT_ITEM_UNPINNED = "context.item_unpinned"
    CONTEXT_ITEM_STALE = "context.item_stale"
    CONTEXT_ITEM_SUPERSEDED = "context.item_superseded"
    CONTEXT_BUNDLE_BUILT = "context.bundle_built"
    CONTEXT_BUNDLE_OVERFLOW = "context.bundle_overflow"
    CONTEXT_SNAPSHOT_SAVED = "context.snapshot_saved"
    CONTEXT_COMPACTION_REQUESTED = "context.compaction_requested"
    CONTEXT_COMPACTION_COMPLETED = "context.compaction_completed"
    CONTEXT_COMPACTION_FAILED = "context.compaction_failed"
    CONTEXT_REFERENCE_RESOLVED = "context.reference_resolved"
    CONTEXT_REFERENCE_STALE = "context.reference_stale"
    CONTEXT_RECOVERY_STARTED = "context.recovery_started"
    CONTEXT_RECOVERY_COMPLETED = "context.recovery_completed"
    INSTRUCTION_SOURCES_COLLECTED = "instruction.sources.collected"
    INSTRUCTION_CONFLICT_DETECTED = "instruction.conflict.detected"
    INSTRUCTION_INJECTION_DETECTED = "instruction.injection_detected"
    PROMPT_COMPILED = "prompt.compiled"
    PROMPT_MANIFEST_CREATED = "prompt.manifest.created"
    PROJECT_INDEX_BUILD_STARTED = "project_index.build_started"
    PROJECT_INDEX_BUILD_COMPLETED = "project_index.build_completed"
    PROJECT_INDEX_BUILD_FAILED = "project_index.build_failed"
    PROJECT_INDEX_REFRESHED = "project_index.refreshed"
    PROJECT_INDEX_UPDATED = "project_index.updated"
    KERNEL_BOOT_STARTED = "kernel.boot.started"
    KERNEL_BOOT_COMPLETED = "kernel.boot.completed"
    KERNEL_BOOT_FAILED = "kernel.boot.failed"
    COMPONENT_INITIALIZED = "component.initialized"
    COMPONENT_HEALTH_CHECKED = "component.health_checked"
    LIFECYCLE_RUN_STARTED = "lifecycle.run.started"
    LIFECYCLE_SESSION_STARTED = "lifecycle.session.started"
    LIFECYCLE_TASK_STARTED = "lifecycle.task.started"
    CANCELLATION_REQUESTED = "cancellation.requested"
    SHUTDOWN_STARTED = "shutdown.started"
    SHUTDOWN_COMPLETED = "shutdown.completed"
    RECOVERY_DETECTED = "recovery.detected"
    RECOVERY_COMPLETED = "recovery.completed"
    FINALIZATION_COMPLETED = "finalization.completed"
    FINAL_REPORT_CREATED = "final_report.created"
    FINAL_REPORT_SECTION_ADDED = "final_report.section_added"
    FINAL_REPORT_COMPLETED = "final_report.completed"


class TraceStatus(str, Enum):
    RUNNING = "running"
    SUCCESS = "success"
    FAILED = "failed"
    CANCELLED = "cancelled"
    TIMEOUT = "timeout"
    SKIPPED = "skipped"
    BLOCKED = "blocked"


class TraceArtifactKind(str, Enum):
    STDOUT = "stdout"
    STDERR = "stderr"
    DIFF = "diff"
    REPORT = "report"
    SNAPSHOT = "snapshot"
    SANDBOX = "sandbox"
    VERIFICATION = "verification"
    EDIT_PLAN = "edit_plan"
    MODEL_MESSAGE = "model_message"
    PROMPT_MANIFEST = "prompt_manifest"
    COMMAND_LOG = "command_log"
    POLICY_AUDIT_REF = "policy_audit_ref"
    GENERIC = "generic"


@dataclass(frozen=True)
class TraceEvent:
    event_id: str
    event_type: TraceEventType
    run_id: str
    session_id: str
    task_id: str | None
    phase_id: str | None
    action_id: str | None
    parent_event_id: str | None
    timestamp: datetime
    monotonic_ms: int
    component: str
    severity: TraceSeverity
    summary: str
    payload: dict[str, Any] = field(default_factory=dict)
    artifact_refs: list[str] = field(default_factory=list)
    policy_decision_id: str | None = None
    approval_grant_id: str | None = None
    sandbox_id: str | None = None
    command_id: str | None = None
    transaction_id: str | None = None
    verification_id: str | None = None
    span_id: str | None = None
    redaction_applied: bool = True
    payload_hash: str = ""

    def __post_init__(self) -> None:
        object.__setattr__(self, "event_type", _enum(TraceEventType, self.event_type))
        object.__setattr__(self, "severity", _enum(TraceSeverity, self.severity))
        object.__setattr__(self, "timestamp", _datetime(self.timestamp))

    def to_dict(self) -> dict[str, Any]:
        event_type = _enum(TraceEventType, self.event_type)
        severity = _enum(TraceSeverity, self.severity)
        return {
            "event_id": self.event_id,
            "event_type": event_type.value,
            "run_id": self.run_id,
            "session_id": self.session_id,
            "task_id": self.task_id,
            "phase_id": self.phase_id,
            "action_id": self.action_id,
            "parent_event_id": self.parent_event_id,
            "timestamp": self.timestamp.isoformat(),
            "monotonic_ms": self.monotonic_ms,
            "component": self.component,
            "severity": severity.value,
            "summary": self.summary,
            "payload": self.payload,
            "artifact_refs": self.artifact_refs,
            "policy_decision_id": self.policy_decision_id,
            "approval_grant_id": self.approval_grant_id,
            "sandbox_id": self.sandbox_id,
            "command_id": self.command_id,
            "transaction_id": self.transaction_id,
            "verification_id": self.verification_id,
            "span_id": self.span_id,
            "redaction_applied": self.redaction_applied,
            "payload_hash": self.payload_hash,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "TraceEvent":
        return cls(**payload)

    def to_json(self) -> str:
        return json.dumps(self.to_dict(), ensure_ascii=False, sort_keys=True, default=str)

    @classmethod
    def from_json(cls, text: str) -> "TraceEvent":
        try:
            return cls.from_dict(json.loads(text))
        except Exception as exc:
            raise TraceSerializationError(str(exc)) from exc


@dataclass(frozen=True)
class TraceSpan:
    span_id: str
    parent_span_id: str | None
    run_id: str
    session_id: str
    task_id: str | None
    phase_id: str | None
    action_id: str | None
    name: str
    component: str
    started_at: datetime
    ended_at: datetime | None
    duration_ms: int | None
    status: TraceStatus
    error_type: str | None
    error_message: str | None
    attributes: dict[str, Any] = field(default_factory=dict)
    artifact_refs: list[str] = field(default_factory=list)

    def __post_init__(self) -> None:
        object.__setattr__(self, "started_at", _datetime(self.started_at))
        if self.ended_at is not None:
            object.__setattr__(self, "ended_at", _datetime(self.ended_at))
        object.__setattr__(self, "status", _enum(TraceStatus, self.status))

    def to_dict(self) -> dict[str, Any]:
        status = _enum(TraceStatus, self.status)
        return {
            "span_id": self.span_id,
            "parent_span_id": self.parent_span_id,
            "run_id": self.run_id,
            "session_id": self.session_id,
            "task_id": self.task_id,
            "phase_id": self.phase_id,
            "action_id": self.action_id,
            "name": self.name,
            "component": self.component,
            "started_at": self.started_at.isoformat(),
            "ended_at": self.ended_at.isoformat() if self.ended_at else None,
            "duration_ms": self.duration_ms,
            "status": status.value,
            "error_type": self.error_type,
            "error_message": self.error_message,
            "attributes": self.attributes,
            "artifact_refs": self.artifact_refs,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "TraceSpan":
        return cls(**payload)


@dataclass(frozen=True)
class TraceArtifact:
    artifact_id: str
    run_id: str
    session_id: str
    task_id: str | None
    kind: TraceArtifactKind
    path: Path
    relative_path: str
    size_bytes: int
    sha256: str
    content_type: str
    redacted: bool
    sensitive: bool
    summary: str
    metadata: dict[str, Any] = field(default_factory=dict)

    def __post_init__(self) -> None:
        object.__setattr__(self, "kind", _enum(TraceArtifactKind, self.kind))
        object.__setattr__(self, "path", Path(self.path))

    def to_dict(self) -> dict[str, Any]:
        kind = _enum(TraceArtifactKind, self.kind)
        return {
            "artifact_id": self.artifact_id,
            "artifact_ref": self.artifact_id,
            "run_id": self.run_id,
            "session_id": self.session_id,
            "task_id": self.task_id,
            "kind": kind.value,
            "relative_path": self.relative_path,
            "relative_handle": self.relative_path,
            "size_bytes": self.size_bytes,
            "sha256": self.sha256,
            "content_type": self.content_type,
            "redacted": self.redacted,
            "sensitive": self.sensitive,
            "summary": self.summary,
            "metadata": self.metadata,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "TraceArtifact":
        data = dict(payload)
        data.pop("artifact_ref", None)
        data.pop("relative_handle", None)
        if "path" not in data:
            data["path"] = data.get("relative_path") or data.get("artifact_id")
        return cls(**data)


@dataclass(frozen=True)
class TraceTimelineItem:
    timestamp: datetime
    event_id: str
    event_type: str
    component: str
    summary: str
    severity: str
    related_ids: list[str] = field(default_factory=list)
    artifact_refs: list[str] = field(default_factory=list)

    def __post_init__(self) -> None:
        object.__setattr__(self, "timestamp", _datetime(self.timestamp))

    def to_dict(self) -> dict[str, Any]:
        return {
            "timestamp": self.timestamp.isoformat(),
            "event_id": self.event_id,
            "event_type": self.event_type,
            "component": self.component,
            "summary": self.summary,
            "severity": self.severity,
            "related_ids": self.related_ids,
            "artifact_refs": self.artifact_refs,
        }


@dataclass(frozen=True)
class TraceSummary:
    run_id: str | None
    session_id: str | None
    task_id: str | None
    total_events: int
    total_spans: int
    total_artifacts: int
    action_count: int
    failed_action_count: int
    command_count: int
    sandboxed_command_count: int
    mutation_count: int
    verification_count: int
    policy_denial_count: int
    approval_count: int
    replan_count: int
    error_count: int
    critical_events: list[dict[str, Any]] = field(default_factory=list)
    key_artifacts: list[str] = field(default_factory=list)
    model_usage_summary: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "run_id": self.run_id,
            "session_id": self.session_id,
            "task_id": self.task_id,
            "total_events": self.total_events,
            "total_spans": self.total_spans,
            "total_artifacts": self.total_artifacts,
            "action_count": self.action_count,
            "failed_action_count": self.failed_action_count,
            "command_count": self.command_count,
            "sandboxed_command_count": self.sandboxed_command_count,
            "mutation_count": self.mutation_count,
            "verification_count": self.verification_count,
            "policy_denial_count": self.policy_denial_count,
            "approval_count": self.approval_count,
            "replan_count": self.replan_count,
            "error_count": self.error_count,
            "critical_events": self.critical_events,
            "key_artifacts": self.key_artifacts,
            "model_usage_summary": self.model_usage_summary,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "TraceSummary":
        return cls(**payload)


def _enum(enum_type: type[_EnumT], value: _EnumT | str) -> _EnumT:
    if isinstance(value, enum_type):
        return value
    text = str(value)
    if text in enum_type.__members__:
        return enum_type[text]
    return enum_type(text)


def _datetime(value: datetime | str) -> datetime:
    if isinstance(value, datetime):
        return value
    parsed = datetime.fromisoformat(str(value))
    if parsed.tzinfo is None:
        return parsed.replace(tzinfo=UTC)
    return parsed
