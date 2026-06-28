from __future__ import annotations

from dataclasses import dataclass, field
from datetime import UTC, datetime
from enum import StrEnum
from pathlib import Path
from typing import Any
from uuid import uuid4


class KernelStatus(StrEnum):
    NEW = "new"
    BOOTING = "booting"
    READY = "ready"
    RUNNING = "running"
    CANCELLING = "cancelling"
    SHUTTING_DOWN = "shutting_down"
    FINALIZED = "finalized"
    FAILED = "failed"


class RunStatus(StrEnum):
    CREATED = "created"
    RUNNING = "running"
    COMPLETED = "completed"
    BLOCKED = "blocked"
    FAILED = "failed"
    CANCELLED = "cancelled"


class SessionStatus(StrEnum):
    CREATED = "created"
    ACTIVE = "active"
    CLOSING = "closing"
    CLOSED = "closed"
    FAILED = "failed"
    CANCELLED = "cancelled"
    RECOVERED = "recovered"


class ShutdownReason(StrEnum):
    NORMAL = "normal"
    BLOCKED = "blocked"
    CANCELLED = "cancelled"
    ERROR = "error"
    BOOTSTRAP_FAILED = "bootstrap_failed"
    KEYBOARD_INTERRUPT = "keyboard_interrupt"


class CancellationReason(StrEnum):
    USER_INTERRUPTED = "user_interrupted"
    SHUTDOWN_REQUESTED = "shutdown_requested"
    POLICY_ABORT = "policy_abort"
    HEALTH_FAILURE = "health_failure"
    INTERNAL_ERROR = "internal_error"


class ComponentName(StrEnum):
    CONFIGURATION = "config"
    OBSERVABILITY = "trace"
    INTERACTION = "interaction"
    WORKSPACE_STATE = "workspace"
    PROJECT_INDEX = "project_index"
    MEMORY = "memory"
    POLICY = "policy"
    SANDBOX = "sandbox"
    COMMAND = "command"
    MUTATION = "mutation"
    EDIT = "edit"
    TOOLS = "tools"
    PLUGINS = "plugins"
    TOOL_EXECUTOR = "tool_executor"
    TOOL_PROTOCOL = "tool_protocol"
    VERIFICATION = "verification"
    REVIEW = "review"
    EVALUATION = "evaluation"
    INSTRUCTIONS = "instructions"
    MODEL = "model"
    CONTEXT = "context"
    PLANNER = "planner"


class ComponentState(StrEnum):
    PENDING = "pending"
    INITIALIZED = "initialized"
    READY = "ready"
    FAILED = "failed"
    STOPPED = "stopped"


@dataclass(frozen=True)
class RunIdentity:
    run_id: str
    session_id: str
    task_id: str

    @classmethod
    def new(
        cls,
        *,
        run_id: str | None = None,
        session_id: str | None = None,
        task_id: str | None = None,
    ) -> RunIdentity:
        base = uuid4().hex[:12]
        resolved_run_id = run_id or f"run_{base}"
        return cls(
            run_id=resolved_run_id,
            session_id=session_id or f"session_{base}",
            task_id=task_id or f"task_{base}",
        )

    def to_dict(self) -> dict[str, str]:
        return {
            "run_id": self.run_id,
            "session_id": self.session_id,
            "task_id": self.task_id,
        }


@dataclass
class AgentRun:
    identity: RunIdentity
    user_goal: str
    status: RunStatus = RunStatus.CREATED
    started_at: str = field(default_factory=lambda: _now())
    ended_at: str | None = None
    final_answer: str | None = None
    error: dict[str, Any] | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            **self.identity.to_dict(),
            "user_goal": self.user_goal,
            "status": self.status.value,
            "started_at": self.started_at,
            "ended_at": self.ended_at,
            "final_answer": self.final_answer,
            "error": self.error,
        }


@dataclass
class AgentSession:
    identity: RunIdentity
    status: SessionStatus = SessionStatus.CREATED
    started_at: str = field(default_factory=lambda: _now())
    ended_at: str | None = None
    recovered_previous_run: bool = False

    def to_dict(self) -> dict[str, Any]:
        return {
            **self.identity.to_dict(),
            "status": self.status.value,
            "started_at": self.started_at,
            "ended_at": self.ended_at,
            "recovered_previous_run": self.recovered_previous_run,
        }


@dataclass
class KernelContext:
    project_root: Path
    identity: RunIdentity
    run: AgentRun
    session: AgentSession | None = None
    status: KernelStatus = KernelStatus.NEW
    components: dict[ComponentName, ComponentState] = field(default_factory=dict)
    diagnostics: list[dict[str, Any]] = field(default_factory=list)
    workspace_lock_status: str = "not_acquired"
    recovered_previous_run: bool = False
    uncertain_transactions: list[str] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {
            "project_root": str(self.project_root),
            "identity": self.identity.to_dict(),
            "run": self.run.to_dict(),
            "session": self.session.to_dict() if self.session else None,
            "status": self.status.value,
            "components": {name.value: state.value for name, state in self.components.items()},
            "diagnostics": self.diagnostics,
            "workspace_lock_status": self.workspace_lock_status,
            "recovered_previous_run": self.recovered_previous_run,
            "uncertain_transactions": self.uncertain_transactions,
        }


@dataclass(frozen=True)
class LifecycleEvent:
    event_type: str
    run_id: str
    session_id: str | None
    task_id: str | None
    timestamp: str = field(default_factory=lambda: _now())
    payload: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_identity(
        cls,
        event_type: str,
        identity: RunIdentity,
        *,
        payload: dict[str, Any] | None = None,
    ) -> LifecycleEvent:
        return cls(
            event_type=event_type,
            run_id=identity.run_id,
            session_id=identity.session_id,
            task_id=identity.task_id,
            payload=payload or {},
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "event_type": self.event_type,
            "run_id": self.run_id,
            "session_id": self.session_id,
            "task_id": self.task_id,
            "timestamp": self.timestamp,
            "payload": self.payload,
        }


def _now() -> str:
    return datetime.now(UTC).isoformat()
