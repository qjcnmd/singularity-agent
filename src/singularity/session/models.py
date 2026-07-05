from __future__ import annotations

from dataclasses import dataclass, field
from datetime import UTC, datetime
from enum import StrEnum
from pathlib import Path
from typing import Any

from singularity.recovery_context import SessionResumeContext


class SessionStatus(StrEnum):
    ACTIVE = "active"
    COMPLETED = "completed"
    BLOCKED = "blocked"
    FAILED = "failed"
    CANCELLED = "cancelled"
    INTERRUPTED = "interrupted"
    NEEDS_REVIEW = "needs_review"


class SessionState(StrEnum):
    ACTIVE = "active"
    RECOVERABLE = "recoverable"
    NEEDS_REVIEW = "needs_review"
    BLOCKED = "blocked"
    CLOSED = "closed"


class SessionRunMode(StrEnum):
    NEW = "new"
    CONTINUE = "continue"
    RESUME = "resume"


class SessionCheckpointKind(StrEnum):
    WORKSPACE = "workspace"
    CONTEXT = "context"
    TOOL_PROTOCOL = "tool_protocol"
    VERIFICATION = "verification"
    RECOVERY_GATE = "recovery_gate"


class RecoveryGateStatus(StrEnum):
    READY_TO_CONTINUE = "ready_to_continue"
    READY_TO_RESUME = "ready_to_resume"
    NEEDS_REVIEW = "needs_review"
    BLOCKED = "blocked"


@dataclass(frozen=True)
class SessionSummary:
    session_id: str
    project_root: str
    user_goal: str
    task_id: str
    status: SessionStatus
    state: SessionState
    created_at: str
    updated_at: str
    last_run_id: str | None = None
    last_task_status: str | None = None
    continue_command: str = ""
    resume_command: str = ""
    show_command: str = ""

    def to_dict(self) -> dict[str, Any]:
        return {
            "session_id": self.session_id,
            "project_root": self.project_root,
            "user_goal": self.user_goal,
            "task_id": self.task_id,
            "status": self.status.value,
            "state": self.state.value,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "last_run_id": self.last_run_id,
            "last_task_status": self.last_task_status,
            "continue_command": self.continue_command,
            "resume_command": self.resume_command,
            "show_command": self.show_command,
        }


@dataclass(frozen=True)
class SessionRun:
    run_id: str
    session_id: str
    task_id: str
    mode: SessionRunMode
    user_goal: str
    trace_run_dir: str
    status: SessionStatus
    started_at: str
    ended_at: str | None = None
    final_report_ref: str | None = None
    summary: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "run_id": self.run_id,
            "session_id": self.session_id,
            "task_id": self.task_id,
            "mode": self.mode.value,
            "user_goal": self.user_goal,
            "trace_run_dir": self.trace_run_dir,
            "status": self.status.value,
            "started_at": self.started_at,
            "ended_at": self.ended_at,
            "final_report_ref": self.final_report_ref,
            "summary": self.summary,
        }


@dataclass(frozen=True)
class SessionCheckpoint:
    checkpoint_id: str
    session_id: str
    run_id: str
    task_id: str
    kind: SessionCheckpointKind
    summary: str
    payload: dict[str, Any]
    created_at: str

    def to_dict(self) -> dict[str, Any]:
        return {
            "checkpoint_id": self.checkpoint_id,
            "session_id": self.session_id,
            "run_id": self.run_id,
            "task_id": self.task_id,
            "kind": self.kind.value,
            "summary": self.summary,
            "payload": self.payload,
            "created_at": self.created_at,
        }


@dataclass(frozen=True)
class SessionTimelineEvent:
    event_id: str
    session_id: str
    run_id: str | None
    task_id: str | None
    event_type: str
    summary: str
    payload: dict[str, Any]
    created_at: str

    def to_dict(self) -> dict[str, Any]:
        return {
            "event_id": self.event_id,
            "session_id": self.session_id,
            "run_id": self.run_id,
            "task_id": self.task_id,
            "event_type": self.event_type,
            "summary": self.summary,
            "payload": self.payload,
            "created_at": self.created_at,
        }


@dataclass(frozen=True)
class SessionDetail:
    session: SessionSummary
    runs: list[SessionRun]
    checkpoints: list[SessionCheckpoint]
    timeline: list[SessionTimelineEvent]

    def to_dict(self) -> dict[str, Any]:
        return {
            "session": self.session.to_dict(),
            "runs": [run.to_dict() for run in self.runs],
            "checkpoints": [checkpoint.to_dict() for checkpoint in self.checkpoints],
            "timeline": [event.to_dict() for event in self.timeline],
        }


@dataclass(frozen=True)
class RecoveryGateDecision:
    session_id: str
    mode: str
    status: RecoveryGateStatus
    can_call_model: bool
    blockers: list[str]
    warnings: list[str]
    next_action: str
    resume_context: SessionResumeContext

    def to_dict(self) -> dict[str, Any]:
        return {
            "session_id": self.session_id,
            "mode": self.mode,
            "status": self.status.value,
            "can_call_model": self.can_call_model,
            "blockers": self.blockers,
            "warnings": self.warnings,
            "next_action": self.next_action,
            "resume_context": self.resume_context.to_dict(),
        }


@dataclass(frozen=True)
class SessionLaunch:
    session_id: str
    task_id: str
    run_id: str
    mode: SessionRunMode
    user_goal: str
    previous_run_id: str | None = None
    previous_status: str | None = None
    previous_trace_run_dir: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "session_id": self.session_id,
            "task_id": self.task_id,
            "run_id": self.run_id,
            "mode": self.mode.value,
            "user_goal": self.user_goal,
            "previous_run_id": self.previous_run_id,
            "previous_status": self.previous_status,
            "previous_trace_run_dir": self.previous_trace_run_dir,
        }


def now_iso() -> str:
    return datetime.now(UTC).isoformat()


def normalize_path(path: Path | str) -> str:
    return str(Path(path).expanduser().resolve(strict=False))


def session_state_for_status(status: SessionStatus) -> SessionState:
    if status == SessionStatus.INTERRUPTED:
        return SessionState.RECOVERABLE
    if status == SessionStatus.NEEDS_REVIEW:
        return SessionState.NEEDS_REVIEW
    if status in {SessionStatus.BLOCKED, SessionStatus.FAILED, SessionStatus.CANCELLED}:
        return SessionState.BLOCKED
    if status == SessionStatus.COMPLETED:
        return SessionState.CLOSED
    return SessionState.ACTIVE
