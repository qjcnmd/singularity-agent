from __future__ import annotations

from typing import Any

from singularity.kernel.models import (
    AgentRun,
    AgentSession,
    LifecycleEvent,
    RunIdentity,
    RunStatus,
    SessionStatus,
)
from singularity.utils.serialization import utc_iso_timestamp


class RunLifecycleManager:
    def __init__(
        self,
        *,
        identity: RunIdentity | None = None,
        trace: Any | None = None,
    ) -> None:
        self.identity = identity or RunIdentity.new()
        self.trace = trace
        self.run: AgentRun | None = None
        self.session: AgentSession | None = None
        self.events: list[LifecycleEvent] = []

    def create_run(self, user_goal: str) -> AgentRun:
        if self.run is not None:
            raise ValueError("Run already exists.")
        self.run = AgentRun(identity=self.identity, user_goal=user_goal, status=RunStatus.RUNNING)
        self._event("lifecycle.run.started", {"user_goal": user_goal})
        return self.run

    def start_session(self) -> AgentSession:
        if self.run is None:
            raise ValueError("Run must be created before session start.")
        if self.session is not None:
            raise ValueError("Session already exists.")
        self.session = AgentSession(identity=self.identity, status=SessionStatus.ACTIVE)
        self._event("lifecycle.session.started")
        return self.session

    def start_task(self, user_goal: str) -> AgentRun:
        if self.run is None or self.session is None:
            raise ValueError("Run and session must exist before task start.")
        self.run.user_goal = user_goal
        self.run.status = RunStatus.RUNNING
        self._event("lifecycle.task.started", {"user_goal": user_goal})
        return self.run

    def mark_completed(self, final_answer: str | None = None) -> AgentRun:
        run = self._run()
        run.status = RunStatus.COMPLETED
        run.ended_at = _now()
        run.final_answer = final_answer
        if self.session is not None:
            self.session.status = SessionStatus.CLOSED
            self.session.ended_at = _now()
        self._event("lifecycle.run.completed", {"final_answer_present": final_answer is not None})
        return run

    def mark_failed(self, error: BaseException | str) -> AgentRun:
        run = self._run()
        run.status = RunStatus.FAILED
        run.ended_at = _now()
        run.error = {
            "type": type(error).__name__ if isinstance(error, BaseException) else "Error",
            "message": str(error),
        }
        if self.session is not None:
            self.session.status = SessionStatus.FAILED
            self.session.ended_at = _now()
        self._event("lifecycle.run.failed", {"error": run.error})
        return run

    def mark_cancelled(self, reason: str = "cancelled") -> AgentRun:
        run = self._run()
        run.status = RunStatus.CANCELLED
        run.ended_at = _now()
        if self.session is not None:
            self.session.status = SessionStatus.CANCELLED
            self.session.ended_at = _now()
        self._event("lifecycle.run.cancelled", {"reason": reason})
        return run

    def mark_blocked(self, reason: str) -> AgentRun:
        run = self._run()
        run.status = RunStatus.BLOCKED
        run.ended_at = _now()
        run.error = {"type": "Blocked", "message": reason}
        if self.session is not None:
            self.session.status = SessionStatus.CLOSED
            self.session.ended_at = _now()
        self._event("lifecycle.run.blocked", {"reason": reason})
        return run

    def summary(self) -> dict[str, Any]:
        return {
            "run_status": self.run.status.value if self.run else None,
            "session_status": self.session.status.value if self.session else None,
            "events": len(self.events),
            "event_types": [event.event_type for event in self.events],
        }

    def _event(self, event_type: str, payload: dict[str, Any] | None = None) -> None:
        event = LifecycleEvent.from_identity(event_type, self.identity, payload=payload)
        self.events.append(event)
        if self.trace is not None and hasattr(self.trace, "record"):
            self.trace.record("lifecycle", event.to_dict())

    def _run(self) -> AgentRun:
        if self.run is None:
            raise ValueError("Run has not been created.")
        return self.run


_now = utc_iso_timestamp
