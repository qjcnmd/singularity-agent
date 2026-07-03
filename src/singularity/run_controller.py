from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass, field
from enum import StrEnum
from typing import Any, TypeVar

from singularity.context.models import ToolObservation
from singularity.error_codes import (
    ErrorCode,
)
from singularity.execution_outcome import ExecutionOutcome, ExecutionOutcomeStatus
from singularity.planner import Planner, PlannerStore
from singularity.status_mapping import (
    PROTOCOL_TERMINAL_RETRY_STATUSES,
    execution_outcome_is_terminal,
    lifecycle_status_for_protocol_next_action,
    protocol_error_code_to_outcome,
)


class RunLifecycleStatus(StrEnum):
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


class RunControlEventKind(StrEnum):
    TASK_STARTED = "task_started"
    OUTCOME_RECORDED = "outcome_recorded"
    PROTOCOL_NEXT_ACTION = "protocol_next_action"
    USER_INPUT_RESUMED = "user_input_resumed"
    CHECKPOINT_SAVED = "checkpoint_saved"
    RESUMED = "resumed"
    COMPLETED = "completed"
    CANCELLED = "cancelled"


@dataclass(frozen=True)
class RunControlEvent:
    kind: RunControlEventKind
    from_status: RunLifecycleStatus
    to_status: RunLifecycleStatus
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


class RunOutcomeReducer:
    def reduce_outcome(
        self,
        current_status: RunLifecycleStatus,
        outcome: ExecutionOutcome,
    ) -> RunControlEvent:
        status = ExecutionOutcomeStatus(outcome.status)
        to_status = current_status
        terminal = False
        if status == ExecutionOutcomeStatus.APPROVAL_REQUIRED:
            to_status = RunLifecycleStatus.WAITING_APPROVAL
        elif status == ExecutionOutcomeStatus.USER_INPUT_REQUIRED:
            to_status = RunLifecycleStatus.WAITING_USER
        elif status in {ExecutionOutcomeStatus.RETRYABLE, ExecutionOutcomeStatus.REPLAN_REQUIRED}:
            to_status = self._running_status_for_nonterminal(outcome, current_status)
        elif status == ExecutionOutcomeStatus.SUCCESS:
            to_status = (
                RunLifecycleStatus.COMPLETED
                if outcome.next_action == "finalize"
                else RunLifecycleStatus.RUNNING
            )
            terminal = to_status == RunLifecycleStatus.COMPLETED
        elif status == ExecutionOutcomeStatus.BLOCKED:
            to_status = RunLifecycleStatus.BLOCKED
            terminal = execution_outcome_is_terminal(status)
        elif status == ExecutionOutcomeStatus.FATAL:
            to_status = RunLifecycleStatus.FAILED
            terminal = execution_outcome_is_terminal(status)
        return RunControlEvent(
            kind=RunControlEventKind.OUTCOME_RECORDED,
            from_status=current_status,
            to_status=to_status,
            reason=outcome.reason,
            terminal=terminal,
            metadata={"execution_outcome": outcome.to_dict()},
        )

    def reduce_protocol_result(
        self,
        current_status: RunLifecycleStatus,
        protocol_result: Any,
        *,
        observations: list[ToolObservation] | None = None,
    ) -> RunControlEvent:
        next_action = str(getattr(protocol_result, "next_action", "") or "request_model")
        pending = int(getattr(protocol_result, "pending_approval_count", 0) or 0)
        to_status = self._status_for_protocol_next_action(next_action, pending, current_status)
        outcome = self.protocol_result_to_outcome(
            protocol_result,
            observations=observations or [],
        )
        return RunControlEvent(
            kind=RunControlEventKind.PROTOCOL_NEXT_ACTION,
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
                "execution_outcome": outcome.to_dict() if outcome is not None else None,
            },
        )

    def protocol_result_to_outcome(
        self,
        protocol_result: Any,
        *,
        observations: list[ToolObservation],
    ) -> ExecutionOutcome | None:
        error_codes = [
            str(observation.error_code)
            for observation in observations
            if getattr(observation, "error_code", None)
        ]
        next_action = str(getattr(protocol_result, "next_action", "") or "continue")
        status = str(
            getattr(
                getattr(protocol_result, "status", None),
                "value",
                getattr(protocol_result, "status", ""),
            )
        )
        failed_count = int(getattr(protocol_result, "failed_count", 0) or 0)
        rejected_count = int(getattr(protocol_result, "rejected_count", 0) or 0)
        pending_count = int(getattr(protocol_result, "pending_approval_count", 0) or 0)
        summary = self._protocol_observation_summary(observations, protocol_result)

        if pending_count or next_action == "pending_approval" or ErrorCode.APPROVAL_REQUIRED.value in error_codes:
            mapping = protocol_error_code_to_outcome(ErrorCode.APPROVAL_REQUIRED.value)
            assert mapping is not None
            return ExecutionOutcome(
                status=mapping.status,
                source=mapping.source,
                reason="Tool execution is waiting for approval.",
                error_code=mapping.error_code,
                next_action=mapping.next_action,
                observation_summary=summary,
                retry_allowed=mapping.retry_allowed,
            )
        if ErrorCode.POLICY_ASK_USER_REQUIRED.value in error_codes:
            mapping = protocol_error_code_to_outcome(ErrorCode.POLICY_ASK_USER_REQUIRED.value)
            assert mapping is not None
            return ExecutionOutcome(
                status=mapping.status,
                source=mapping.source,
                reason="Policy requires user input.",
                error_code=mapping.error_code,
                next_action=mapping.next_action,
                observation_summary=summary,
                retry_allowed=mapping.retry_allowed,
            )
        for target_status, reason_template in (
            (ExecutionOutcomeStatus.BLOCKED, "Tool execution blocked: {error_code}."),
            (ExecutionOutcomeStatus.REPLAN_REQUIRED, "Tool result requires replanning: {error_code}."),
            (ExecutionOutcomeStatus.RETRYABLE, "Tool call can be retried after correction: {error_code}."),
        ):
            outcome = self._protocol_mapping_outcome(
                error_codes,
                target_status=target_status,
                reason_template=reason_template,
                observation_summary=summary,
            )
            if outcome is not None:
                return outcome
        if next_action == "fail_safe" or status in PROTOCOL_TERMINAL_RETRY_STATUSES:
            metadata = getattr(protocol_result, "metadata", {}) or {}
            reason = metadata.get("reason") if isinstance(metadata, dict) else None
            return ExecutionOutcome(
                status=ExecutionOutcomeStatus.RETRYABLE,
                source="protocol",
                reason="Protocol fail-safe requested another model turn.",
                error_code=str(reason or ErrorCode.PROTOCOL_FAIL_SAFE.value),
                next_action="retry",
                observation_summary=summary,
                retry_allowed=True,
            )
        if failed_count or rejected_count or next_action == "recover":
            return ExecutionOutcome(
                status=ExecutionOutcomeStatus.RETRYABLE,
                source="protocol",
                reason="Protocol reported recoverable tool failure.",
                error_code=error_codes[0] if error_codes else ErrorCode.TOOL_FAILURE.value,
                next_action="retry",
                observation_summary=summary,
                retry_allowed=True,
            )
        return None

    @staticmethod
    def _protocol_mapping_outcome(
        error_codes: list[str],
        *,
        target_status: ExecutionOutcomeStatus,
        reason_template: str,
        observation_summary: str,
    ) -> ExecutionOutcome | None:
        for code in error_codes:
            mapping = protocol_error_code_to_outcome(code)
            if mapping is None or mapping.status != target_status:
                continue
            return ExecutionOutcome(
                status=mapping.status,
                source=mapping.source,
                reason=reason_template.format(error_code=mapping.error_code),
                error_code=mapping.error_code,
                next_action=mapping.next_action,
                observation_summary=observation_summary,
                retry_allowed=mapping.retry_allowed,
            )
        return None

    @staticmethod
    def _protocol_observation_summary(
        observations: list[ToolObservation],
        protocol_result: Any,
    ) -> str:
        if observations:
            parts: list[str] = []
            for observation in observations[-3:]:
                status = "ok" if observation.ok else (observation.error_code or "failed")
                preview = str(observation.preview or "").replace("\n", " ")[:160]
                parts.append(f"{observation.tool_name}:{status}:{preview}")
            if parts:
                return "; ".join(parts)
        summary = getattr(protocol_result, "summary", None)
        if summary:
            return str(summary)
        return f"Tool Protocol next_action={getattr(protocol_result, 'next_action', 'continue')}"

    @staticmethod
    def _running_status_for_nonterminal(
        outcome: ExecutionOutcome,
        current_status: RunLifecycleStatus,
    ) -> RunLifecycleStatus:
        if outcome.error_code in {
            ErrorCode.VERIFICATION_FAILED.value,
            ErrorCode.BLOCKED_BY_VERIFICATION.value,
            ErrorCode.SEMANTIC_FAILURE.value,
        }:
            return RunLifecycleStatus.REPAIRING
        if outcome.next_action == "replan":
            return RunLifecycleStatus.RUNNING
        return RunLifecycleStatus.RUNNING if current_status != RunLifecycleStatus.VERIFYING else current_status

    @staticmethod
    def _status_for_protocol_next_action(
        next_action: str,
        pending_approval_count: int,
        current_status: RunLifecycleStatus,
    ) -> RunLifecycleStatus:
        mapped = lifecycle_status_for_protocol_next_action(
            next_action,
            pending_approval_count=pending_approval_count,
            current_status=current_status.value,
        )
        return RunLifecycleStatus(mapped)


class RunCheckpointStore:
    def __init__(self, store: PlannerStore) -> None:
        self.store = store

    def checkpoint(self, planner: Planner) -> None:
        planner.checkpoint()

    def load(self, session_id: str):
        return self.store.load(session_id)


T = TypeVar("T")


class RunController:
    def __init__(
        self,
        *,
        planner: Planner,
        trace: Any | None = None,
        reducer: RunOutcomeReducer | None = None,
    ) -> None:
        self.planner = planner
        self.trace = trace
        self.reducer = reducer or RunOutcomeReducer()
        self.state_store = RunCheckpointStore(planner.store)

    def start(self, user_goal: str) -> RunControlEvent:
        if self.planner.state is None:
            self.planner.start_task(user_goal)
        event = RunControlEvent(
            kind=RunControlEventKind.TASK_STARTED,
            from_status=self.current_status,
            to_status=RunLifecycleStatus.RUNNING,
            reason="Task lifecycle started.",
            terminal=False,
        )
        return self.apply_event(event)

    def apply_outcome(self, outcome: ExecutionOutcome | dict[str, Any]) -> RunControlEvent:
        resolved = outcome if isinstance(outcome, ExecutionOutcome) else ExecutionOutcome.from_dict(outcome)
        event = self.reducer.reduce_outcome(self.current_status, resolved)
        self.planner.record_execution_outcome(resolved)
        return self.apply_event(event)

    def apply_protocol_result(
        self,
        protocol_result: Any,
        *,
        observations: list[ToolObservation] | None = None,
    ) -> RunControlEvent:
        event = self.reducer.reduce_protocol_result(
            self.current_status,
            protocol_result,
            observations=observations,
        )
        return self.apply_event(event)

    def reduce_protocol_result(
        self,
        protocol_result: Any,
        *,
        observations: list[ToolObservation] | None = None,
    ) -> ExecutionOutcome | None:
        return self.reducer.protocol_result_to_outcome(
            protocol_result,
            observations=observations or [],
        )

    def dispatch_protocol_recovery(self, recovery_manager: Any, *, run_id: str) -> RunControlEvent:
        task_state = self.planner.state
        result = recovery_manager.recover(
            run_id=run_id,
            session_id=task_state.session_id if task_state else None,
            task_id=task_state.task_id if task_state else None,
        )
        return self.apply_protocol_result(result)

    def resume_user_input(self, answer: Any) -> RunControlEvent:
        event = RunControlEvent(
            kind=RunControlEventKind.USER_INPUT_RESUMED,
            from_status=self.current_status,
            to_status=RunLifecycleStatus.RUNNING,
            reason="User input received; task can continue.",
            terminal=False,
            metadata={"answer_present": answer is not None},
        )
        return self.apply_event(event)

    def checkpoint(self) -> RunControlEvent:
        self.state_store.checkpoint(self.planner)
        event = RunControlEvent(
            kind=RunControlEventKind.CHECKPOINT_SAVED,
            from_status=self.current_status,
            to_status=self.current_status,
            reason="Task checkpoint saved.",
        )
        return self.apply_event(event)

    def resume(self, session_id: str, *, workspace_health: dict[str, Any] | None = None) -> RunControlEvent:
        self.planner.resume(session_id, workspace_health=workspace_health)
        to_status = self.current_status
        if to_status in {RunLifecycleStatus.CREATED, RunLifecycleStatus.CANCELLED}:
            to_status = RunLifecycleStatus.RUNNING
        event = RunControlEvent(
            kind=RunControlEventKind.RESUMED,
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
            self._set_status(RunLifecycleStatus.RUNNING)
        for turn in range(1, max_turns + 1):
            result = run_turn(turn)
            if result is not None:
                if self.current_status in {
                    RunLifecycleStatus.CREATED,
                    RunLifecycleStatus.RUNNING,
                    RunLifecycleStatus.VERIFYING,
                    RunLifecycleStatus.REPAIRING,
                    RunLifecycleStatus.FINAL_REVIEW,
                    RunLifecycleStatus.REPORTING,
                }:
                    self.complete()
                return result
        result = on_max_turns(max_turns)
        self.apply_event(
            RunControlEvent(
                kind=RunControlEventKind.OUTCOME_RECORDED,
                from_status=self.current_status,
                to_status=RunLifecycleStatus.BLOCKED,
                reason=f"Task stopped after max_turns={max_turns}.",
                terminal=True,
                metadata={"error_code": ErrorCode.MAX_TURNS_EXCEEDED.value},
            )
        )
        return result

    def complete(self) -> RunControlEvent:
        event = RunControlEvent(
            kind=RunControlEventKind.COMPLETED,
            from_status=self.current_status,
            to_status=RunLifecycleStatus.COMPLETED,
            reason="Task completed.",
            terminal=True,
        )
        return self.apply_event(event)

    def cancel(self, reason: str = "cancelled") -> RunControlEvent:
        event = RunControlEvent(
            kind=RunControlEventKind.CANCELLED,
            from_status=self.current_status,
            to_status=RunLifecycleStatus.CANCELLED,
            reason=reason,
            terminal=True,
        )
        return self.apply_event(event)

    def apply_event(self, event: RunControlEvent) -> RunControlEvent:
        self._set_status(event.to_status)
        payload = event.to_dict()
        self._record_event(payload)
        return event

    @property
    def current_status(self) -> RunLifecycleStatus:
        state = self.planner.state
        if state is None:
            return RunLifecycleStatus.CREATED
        try:
            return RunLifecycleStatus(state.lifecycle_status)
        except ValueError:
            return RunLifecycleStatus.RUNNING

    def _set_status(self, status: RunLifecycleStatus) -> None:
        if self.planner.state is None:
            return
        self.planner.state.lifecycle_status = status.value
        self.planner.state.touch()
        self.planner.checkpoint()

    def _record_event(self, payload: dict[str, Any]) -> None:
        if self.trace is not None and hasattr(self.trace, "record"):
            self.trace.record("task_lifecycle", payload)
        self.planner.record_task_lifecycle_event(payload)
