from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from typing import Any, Callable, TypeVar

from singularity.execution_outcome import ExecutionOutcome, ExecutionOutcomeStatus
from singularity.planner import PlannerRuntime, PlannerStore


class TaskLifecycleStatus(str, Enum):
    CREATED = "created"
    RUNNING = "running"
    WAITING_USER = "waiting_user"
    WAITING_APPROVAL = "waiting_approval"
    VERIFYING = "verifying"
    REPAIRING = "repairing"
    FINAL_REVIEW = "final_review"
    REPORTING = "reporting"
    COMPLETED = "completed"
    BLOCKED = "blocked"
    FAILED = "failed"
    CANCELLED = "cancelled"


class TaskEventKind(str, Enum):
    TASK_STARTED = "task_started"
    OUTCOME_RECORDED = "outcome_recorded"
    PROTOCOL_NEXT_ACTION = "protocol_next_action"
    USER_INPUT_RESUMED = "user_input_resumed"
    CHECKPOINT_SAVED = "checkpoint_saved"
    RESUMED = "resumed"
    COMPLETED = "completed"
    CANCELLED = "cancelled"


@dataclass(frozen=True)
class TaskEvent:
    kind: TaskEventKind
    from_status: TaskLifecycleStatus
    to_status: TaskLifecycleStatus
    reason: str
    terminal: bool = False
    metadata: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "kind": self.kind.value,
            "from_status": self.from_status.value,
            "to_status": self.to_status.value,
            "reason": self.reason,
            "terminal": self.terminal,
            "metadata": self.metadata,
        }


class OutcomeReducer:
    def reduce_outcome(
        self,
        current_status: TaskLifecycleStatus,
        outcome: ExecutionOutcome,
    ) -> TaskEvent:
        status = ExecutionOutcomeStatus(outcome.status)
        to_status = current_status
        terminal = False
        if status == ExecutionOutcomeStatus.APPROVAL_REQUIRED:
            to_status = TaskLifecycleStatus.WAITING_APPROVAL
        elif status == ExecutionOutcomeStatus.USER_INPUT_REQUIRED:
            to_status = TaskLifecycleStatus.WAITING_USER
        elif status in {ExecutionOutcomeStatus.RETRYABLE, ExecutionOutcomeStatus.REPLAN_REQUIRED}:
            to_status = self._running_status_for_nonterminal(outcome, current_status)
        elif status == ExecutionOutcomeStatus.SUCCESS:
            to_status = (
                TaskLifecycleStatus.COMPLETED
                if outcome.next_action == "finalize"
                else TaskLifecycleStatus.RUNNING
            )
            terminal = to_status == TaskLifecycleStatus.COMPLETED
        elif status == ExecutionOutcomeStatus.BLOCKED:
            to_status = TaskLifecycleStatus.BLOCKED
            terminal = True
        elif status == ExecutionOutcomeStatus.FATAL:
            to_status = TaskLifecycleStatus.FAILED
            terminal = True
        return TaskEvent(
            kind=TaskEventKind.OUTCOME_RECORDED,
            from_status=current_status,
            to_status=to_status,
            reason=outcome.reason,
            terminal=terminal,
            metadata={"execution_outcome": outcome.to_dict()},
        )

    def reduce_protocol_result(
        self,
        current_status: TaskLifecycleStatus,
        protocol_result: Any,
    ) -> TaskEvent:
        next_action = str(getattr(protocol_result, "next_action", "") or "request_model")
        pending = int(getattr(protocol_result, "pending_approval_count", 0) or 0)
        to_status = self._status_for_protocol_next_action(next_action, pending, current_status)
        return TaskEvent(
            kind=TaskEventKind.PROTOCOL_NEXT_ACTION,
            from_status=current_status,
            to_status=to_status,
            reason=f"Tool Protocol next_action={next_action}",
            terminal=False,
            metadata={
                "next_action": next_action,
                "pending_approval_count": pending,
                "protocol_status": str(
                    getattr(getattr(protocol_result, "status", None), "value", getattr(protocol_result, "status", ""))
                ),
            },
        )

    @staticmethod
    def _running_status_for_nonterminal(
        outcome: ExecutionOutcome,
        current_status: TaskLifecycleStatus,
    ) -> TaskLifecycleStatus:
        if outcome.error_code in {"verification_failed", "blocked_by_verification", "semantic_failure"}:
            return TaskLifecycleStatus.REPAIRING
        if outcome.next_action == "replan":
            return TaskLifecycleStatus.RUNNING
        return TaskLifecycleStatus.RUNNING if current_status != TaskLifecycleStatus.VERIFYING else current_status

    @staticmethod
    def _status_for_protocol_next_action(
        next_action: str,
        pending_approval_count: int,
        current_status: TaskLifecycleStatus,
    ) -> TaskLifecycleStatus:
        if pending_approval_count or next_action in {"pending_approval", "resume_pending_approval"}:
            return TaskLifecycleStatus.WAITING_APPROVAL
        if next_action in {"ask_user", "request_user_input"}:
            return TaskLifecycleStatus.WAITING_USER
        if next_action in {"await_tool_result", "execute_pending_tool", "append_tool_message", "request_model", "continue"}:
            return TaskLifecycleStatus.RUNNING
        if next_action == "finalize":
            return TaskLifecycleStatus.REPORTING
        return current_status


class TaskStateStore:
    def __init__(self, store: PlannerStore) -> None:
        self.store = store

    def checkpoint(self, planner: PlannerRuntime) -> None:
        planner._persist()  # type: ignore[attr-defined]

    def load(self, session_id: str):
        return self.store.load(session_id)


T = TypeVar("T")


class TaskController:
    def __init__(
        self,
        *,
        planner: PlannerRuntime,
        trace: Any | None = None,
        reducer: OutcomeReducer | None = None,
    ) -> None:
        self.planner = planner
        self.trace = trace
        self.reducer = reducer or OutcomeReducer()
        self.state_store = TaskStateStore(planner.store)

    def start(self, user_goal: str) -> TaskEvent:
        if self.planner.state is None:
            self.planner.start_task(user_goal)
        event = TaskEvent(
            kind=TaskEventKind.TASK_STARTED,
            from_status=self.current_status,
            to_status=TaskLifecycleStatus.RUNNING,
            reason="Task lifecycle started.",
            terminal=False,
        )
        return self.apply_event(event)

    def apply_outcome(self, outcome: ExecutionOutcome | dict[str, Any]) -> TaskEvent:
        resolved = outcome if isinstance(outcome, ExecutionOutcome) else ExecutionOutcome.from_dict(outcome)
        event = self.reducer.reduce_outcome(self.current_status, resolved)
        self.planner.record_execution_outcome(resolved)
        return self.apply_event(event)

    def apply_protocol_result(self, protocol_result: Any) -> TaskEvent:
        event = self.reducer.reduce_protocol_result(self.current_status, protocol_result)
        return self.apply_event(event)

    def dispatch_protocol_recovery(self, recovery_manager: Any, *, run_id: str) -> TaskEvent:
        task_state = self.planner.state
        result = recovery_manager.recover(
            run_id=run_id,
            session_id=task_state.session_id if task_state else None,
            task_id=task_state.task_id if task_state else None,
        )
        return self.apply_protocol_result(result)

    def resume_user_input(self, answer: Any) -> TaskEvent:
        event = TaskEvent(
            kind=TaskEventKind.USER_INPUT_RESUMED,
            from_status=self.current_status,
            to_status=TaskLifecycleStatus.RUNNING,
            reason="User input received; task can continue.",
            terminal=False,
            metadata={"answer_present": answer is not None},
        )
        return self.apply_event(event)

    def checkpoint(self) -> TaskEvent:
        self.state_store.checkpoint(self.planner)
        event = TaskEvent(
            kind=TaskEventKind.CHECKPOINT_SAVED,
            from_status=self.current_status,
            to_status=self.current_status,
            reason="Task checkpoint saved.",
        )
        return self.apply_event(event)

    def resume(self, session_id: str, *, workspace_health: dict[str, Any] | None = None) -> TaskEvent:
        self.planner.resume(session_id, workspace_health=workspace_health)
        to_status = self.current_status
        if to_status in {TaskLifecycleStatus.CREATED, TaskLifecycleStatus.CANCELLED}:
            to_status = TaskLifecycleStatus.RUNNING
        event = TaskEvent(
            kind=TaskEventKind.RESUMED,
            from_status=self.current_status,
            to_status=to_status,
            reason="Task controller resumed from checkpoint.",
        )
        return self.apply_event(event)

    def run_loop(
        self,
        user_goal: str,
        *,
        max_turns: int,
        run_turn: Callable[[int], T | None],
        on_max_turns: Callable[[int], T],
    ) -> T:
        if self.planner.state is None:
            self.start(user_goal)
        else:
            self._set_status(TaskLifecycleStatus.RUNNING)
        for turn in range(1, max_turns + 1):
            result = run_turn(turn)
            if result is not None:
                if self.current_status in {
                    TaskLifecycleStatus.CREATED,
                    TaskLifecycleStatus.RUNNING,
                    TaskLifecycleStatus.VERIFYING,
                    TaskLifecycleStatus.REPAIRING,
                    TaskLifecycleStatus.FINAL_REVIEW,
                    TaskLifecycleStatus.REPORTING,
                }:
                    self.complete()
                return result
        result = on_max_turns(max_turns)
        self.apply_event(
            TaskEvent(
                kind=TaskEventKind.OUTCOME_RECORDED,
                from_status=self.current_status,
                to_status=TaskLifecycleStatus.BLOCKED,
                reason=f"Task stopped after max_turns={max_turns}.",
                terminal=True,
                metadata={"error_code": "max_turns_exceeded"},
            )
        )
        return result

    def complete(self) -> TaskEvent:
        event = TaskEvent(
            kind=TaskEventKind.COMPLETED,
            from_status=self.current_status,
            to_status=TaskLifecycleStatus.COMPLETED,
            reason="Task completed.",
            terminal=True,
        )
        return self.apply_event(event)

    def cancel(self, reason: str = "cancelled") -> TaskEvent:
        event = TaskEvent(
            kind=TaskEventKind.CANCELLED,
            from_status=self.current_status,
            to_status=TaskLifecycleStatus.CANCELLED,
            reason=reason,
            terminal=True,
        )
        return self.apply_event(event)

    def apply_event(self, event: TaskEvent) -> TaskEvent:
        self._set_status(event.to_status)
        payload = event.to_dict()
        self._record_event(payload)
        return event

    @property
    def current_status(self) -> TaskLifecycleStatus:
        state = self.planner.state
        if state is None:
            return TaskLifecycleStatus.CREATED
        try:
            return TaskLifecycleStatus(state.lifecycle_status)
        except ValueError:
            return TaskLifecycleStatus.RUNNING

    def _set_status(self, status: TaskLifecycleStatus) -> None:
        if self.planner.state is None:
            return
        self.planner.state.lifecycle_status = status.value
        self.planner.state.touch()
        self.planner._persist()  # type: ignore[attr-defined]

    def _record_event(self, payload: dict[str, Any]) -> None:
        if self.trace is not None and hasattr(self.trace, "record"):
            self.trace.record("task_lifecycle", payload)
        if hasattr(self.planner, "_record_event"):
            self.planner._record_event(  # type: ignore[attr-defined]
                decision="task_lifecycle",
                reason=payload["reason"],
                extra={"task_event": payload},
            )
