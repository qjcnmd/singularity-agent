from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from singularity.observability.models import TraceEvent
from singularity.policy import ApprovalGrant


SCHEMA_VERSION = "1.0"


@dataclass(frozen=True)
class RunEvent:
    event_id: str
    event_type: str
    run_id: str
    runtime: str
    severity: str
    timestamp: str
    sequence: int
    summary: str
    session_id: str | None = None
    task_id: str | None = None
    phase_id: str | None = None
    action_id: str | None = None
    payload: dict[str, Any] = field(default_factory=dict)
    artifact_refs: list[str] = field(default_factory=list)
    redaction_applied: bool = True
    schema_version: str = SCHEMA_VERSION

    @classmethod
    def from_trace_event(cls, event: TraceEvent, *, sequence: int) -> "RunEvent":
        return cls(
            event_id=event.event_id,
            event_type=event.event_type.value if hasattr(event.event_type, "value") else str(event.event_type),
            run_id=event.run_id,
            session_id=event.session_id,
            task_id=event.task_id,
            phase_id=event.phase_id,
            action_id=event.action_id,
            runtime=event.runtime,
            severity=event.severity.value if hasattr(event.severity, "value") else str(event.severity),
            timestamp=event.timestamp.isoformat(),
            sequence=sequence,
            summary=event.summary,
            payload=dict(event.payload or {}),
            artifact_refs=list(event.artifact_refs or []),
            redaction_applied=bool(event.redaction_applied),
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "event_id": self.event_id,
            "event_type": self.event_type,
            "schema_version": self.schema_version,
            "run_id": self.run_id,
            "session_id": self.session_id,
            "task_id": self.task_id,
            "phase_id": self.phase_id,
            "action_id": self.action_id,
            "runtime": self.runtime,
            "severity": self.severity,
            "timestamp": self.timestamp,
            "sequence": self.sequence,
            "summary": self.summary,
            "payload": self.payload,
            "artifact_refs": self.artifact_refs,
            "redaction_applied": self.redaction_applied,
        }


@dataclass(frozen=True)
class ApprovalEvent:
    request_id: str
    decision_id: str
    session_id: str
    status: str
    scope: dict[str, Any]
    grant_id: str | None = None
    message: str = ""
    approved_by: str | None = None
    approved_at: str | None = None
    expires_at: str | None = None
    reason: str = ""
    schema_version: str = SCHEMA_VERSION

    @classmethod
    def from_grant(cls, grant: ApprovalGrant, *, status: str = "granted") -> "ApprovalEvent":
        return cls(
            request_id=grant.request_id,
            decision_id=grant.decision_id,
            session_id=grant.session_id or "",
            status=status,
            grant_id=grant.grant_id,
            scope=grant.scope.to_dict(),
            approved_by=grant.approved_by,
            approved_at=grant.approved_at,
            expires_at=grant.expires_at,
            reason=grant.reason,
            message=grant.reason,
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "schema_version": self.schema_version,
            "request_id": self.request_id,
            "decision_id": self.decision_id,
            "grant_id": self.grant_id,
            "session_id": self.session_id,
            "status": self.status,
            "message": self.message,
            "approved_by": self.approved_by,
            "approved_at": self.approved_at,
            "expires_at": self.expires_at,
            "scope": self.scope,
            "reason": self.reason,
        }


@dataclass(frozen=True)
class ToolCallEvent:
    tool_call_id: str
    tool_name: str
    run_id: str
    session_id: str
    task_id: str
    phase: str
    argument_digest: str
    phase_id: str | None = None
    normalized_arguments: dict[str, Any] = field(default_factory=dict)
    policy_decision_id: str | None = None
    approval_grant_id: str | None = None
    result: dict[str, Any] | None = None
    artifact_refs: list[str] = field(default_factory=list)
    error_kind: str | None = None
    redaction_applied: bool = True
    schema_version: str = SCHEMA_VERSION

    @classmethod
    def from_run_event(cls, event: RunEvent) -> "ToolCallEvent":
        payload = dict(event.payload or {})
        return cls(
            tool_call_id=str(payload.get("tool_call_id") or event.action_id or ""),
            tool_name=str(payload.get("tool_name") or payload.get("name") or ""),
            run_id=event.run_id,
            session_id=event.session_id or "",
            task_id=event.task_id or "",
            phase=str(payload.get("phase") or _phase_from_event_type(event.event_type)),
            phase_id=event.phase_id,
            argument_digest=str(payload.get("argument_digest") or payload.get("arguments_hash") or ""),
            normalized_arguments=dict(payload.get("normalized_arguments") or {}),
            policy_decision_id=payload.get("policy_decision_id"),
            approval_grant_id=payload.get("approval_grant_id"),
            result=payload.get("result") if isinstance(payload.get("result"), dict) else None,
            artifact_refs=list(event.artifact_refs or payload.get("artifact_refs") or []),
            error_kind=payload.get("error_kind") or payload.get("error_code"),
            redaction_applied=event.redaction_applied,
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "schema_version": self.schema_version,
            "tool_call_id": self.tool_call_id,
            "tool_name": self.tool_name,
            "run_id": self.run_id,
            "session_id": self.session_id,
            "task_id": self.task_id,
            "phase_id": self.phase_id,
            "phase": self.phase,
            "normalized_arguments": self.normalized_arguments,
            "argument_digest": self.argument_digest,
            "policy_decision_id": self.policy_decision_id,
            "approval_grant_id": self.approval_grant_id,
            "result": self.result,
            "artifact_refs": self.artifact_refs,
            "error_kind": self.error_kind,
            "redaction_applied": self.redaction_applied,
        }


@dataclass
class SessionRuntime:
    run_id: str
    session_id: str
    task_id: str
    status: str
    trace_run_dir: Path
    final_answer: str | None = None
    final_report: dict[str, Any] | None = None

    def to_snapshot(
        self,
        *,
        event_count: int,
        artifact_count: int,
        last_sequence: int | None,
    ) -> "RuntimeHostSnapshot":
        return RuntimeHostSnapshot(
            run_id=self.run_id,
            session_id=self.session_id,
            task_id=self.task_id,
            status=self.status,
            trace_run_dir=str(self.trace_run_dir),
            event_count=event_count,
            artifact_count=artifact_count,
            last_sequence=last_sequence,
            final_answer=self.final_answer,
            final_report=self.final_report,
        )


@dataclass(frozen=True)
class RuntimeHostSnapshot:
    run_id: str
    session_id: str | None
    task_id: str | None
    status: str
    trace_run_dir: str
    event_count: int
    artifact_count: int
    last_sequence: int | None
    final_answer: str | None = None
    final_report: dict[str, Any] | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "run_id": self.run_id,
            "session_id": self.session_id,
            "task_id": self.task_id,
            "status": self.status,
            "trace_run_dir": self.trace_run_dir,
            "event_count": self.event_count,
            "artifact_count": self.artifact_count,
            "last_sequence": self.last_sequence,
            "final_answer": self.final_answer,
            "final_report": self.final_report,
        }


@dataclass(frozen=True)
class RuntimeHostRunResult:
    run_id: str
    session_id: str
    task_id: str
    status: str
    final_answer: str
    final_report: dict[str, Any]
    snapshot: RuntimeHostSnapshot

    def to_dict(self) -> dict[str, Any]:
        return {
            "run_id": self.run_id,
            "session_id": self.session_id,
            "task_id": self.task_id,
            "status": self.status,
            "final_answer": self.final_answer,
            "final_report": self.final_report,
            "snapshot": self.snapshot.to_dict(),
        }


def _phase_from_event_type(event_type: str) -> str:
    if event_type.endswith("_completed") or event_type.endswith(".completed"):
        return "succeeded"
    if event_type.endswith("_started") or event_type.endswith(".started"):
        return "running"
    if event_type.endswith("_validated") or event_type.endswith(".validated"):
        return "validated"
    if event_type.endswith("_rejected") or event_type.endswith(".rejected"):
        return "rejected"
    if event_type.endswith("_scheduled") or event_type.endswith(".scheduled"):
        return "scheduled"
    if event_type.endswith("_failed") or event_type.endswith(".failed"):
        return "failed"
    return "proposed"
