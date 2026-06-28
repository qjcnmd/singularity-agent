from __future__ import annotations

import json
from types import SimpleNamespace

from singularity.execution_outcome import ExecutionOutcome, ExecutionOutcomeStatus
from singularity.planner import Planner
from singularity.run_controller import (
    RunControlEventKind,
    RunController,
    RunLifecycleStatus,
    RunOutcomeReducer,
)


class RecordingTrace:
    def __init__(self) -> None:
        self.events: list[tuple[str, dict]] = []

    def record(self, event: str, data: dict) -> None:
        self.events.append((event, data))


def test_outcome_reducer_maps_waiting_and_nonterminal_outcomes() -> None:
    reducer = RunOutcomeReducer()

    approval = reducer.reduce_outcome(
        RunLifecycleStatus.RUNNING,
        ExecutionOutcome(
            status=ExecutionOutcomeStatus.APPROVAL_REQUIRED,
            source="protocol",
            reason="approval required",
            next_action="wait_for_approval",
            retry_allowed=False,
        ),
    )
    replan = reducer.reduce_outcome(
        RunLifecycleStatus.RUNNING,
        ExecutionOutcome(
            status=ExecutionOutcomeStatus.REPLAN_REQUIRED,
            source="completion",
            reason="missing evidence",
            next_action="continue",
        ),
    )

    assert approval.kind == RunControlEventKind.OUTCOME_RECORDED
    assert approval.to_status == RunLifecycleStatus.WAITING_APPROVAL
    assert approval.terminal is False
    assert replan.to_status == RunLifecycleStatus.RUNNING
    assert replan.terminal is False


def test_protocol_next_action_maps_to_task_event() -> None:
    event = RunOutcomeReducer().reduce_protocol_result(
        RunLifecycleStatus.RUNNING,
        SimpleNamespace(
            next_action="resume_pending_approval",
            pending_approval_count=1,
            status=SimpleNamespace(value="pending_approval"),
        ),
    )

    assert event.kind == RunControlEventKind.PROTOCOL_NEXT_ACTION
    assert event.to_status == RunLifecycleStatus.WAITING_APPROVAL
    assert event.metadata["next_action"] == "resume_pending_approval"


def test_task_controller_records_trace_event_for_state_transition(tmp_path) -> None:
    trace = RecordingTrace()
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    controller = RunController(planner=planner, trace=trace)

    controller.start("create a file")
    event = controller.apply_outcome(
        ExecutionOutcome(
            status=ExecutionOutcomeStatus.USER_INPUT_REQUIRED,
            source="policy",
            reason="need user input",
            error_code="policy_ask_user_required",
            next_action="ask_user",
            retry_allowed=False,
        )
    )

    assert event.to_status == RunLifecycleStatus.WAITING_USER
    assert planner.state is not None
    assert planner.state.lifecycle_status == RunLifecycleStatus.WAITING_USER.value
    context = json.loads(planner.planner_context_message()["content"])["planner"]
    assert context["lifecycle_status"] == RunLifecycleStatus.WAITING_USER.value
    assert trace.events[-1][0] == "task_lifecycle"
    assert trace.events[-1][1]["to_status"] == RunLifecycleStatus.WAITING_USER.value


def test_task_controller_dispatches_protocol_recovery(tmp_path) -> None:
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    controller = RunController(planner=planner)
    controller.start("resume protocol")

    class FakeRecoveryManager:
        def __init__(self) -> None:
            self.calls: list[dict[str, str]] = []

        def recover(self, *, run_id: str, session_id: str | None = None, task_id: str | None = None):
            self.calls.append({"run_id": run_id, "session_id": session_id or "", "task_id": task_id or ""})
            return SimpleNamespace(
                next_action="resume_pending_approval",
                pending_approval_count=1,
                status=SimpleNamespace(value="pending_approval"),
            )

    recovery = FakeRecoveryManager()

    event = controller.dispatch_protocol_recovery(recovery, run_id="run_1")

    assert recovery.calls == [{"run_id": "run_1", "session_id": "session_1", "task_id": "task_1"}]
    assert event.kind == RunControlEventKind.PROTOCOL_NEXT_ACTION
    assert event.to_status == RunLifecycleStatus.WAITING_APPROVAL
    assert planner.state is not None
    assert planner.state.lifecycle_status == RunLifecycleStatus.WAITING_APPROVAL.value


def test_task_controller_resumes_user_input_wait(tmp_path) -> None:
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    controller = RunController(planner=planner)
    controller.start("ask when needed")
    controller.apply_outcome(
        ExecutionOutcome(
            status=ExecutionOutcomeStatus.USER_INPUT_REQUIRED,
            source="policy",
            reason="need user input",
            next_action="ask_user",
            retry_allowed=False,
        )
    )

    event = controller.resume_user_input({"answer": "continue"})

    assert event.kind == RunControlEventKind.USER_INPUT_RESUMED
    assert event.from_status == RunLifecycleStatus.WAITING_USER
    assert event.to_status == RunLifecycleStatus.RUNNING
    assert planner.state is not None
    assert planner.state.lifecycle_status == RunLifecycleStatus.RUNNING.value


def test_task_controller_checkpoint_resume_preserves_waiting_state(tmp_path) -> None:
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    controller = RunController(planner=planner)
    controller.start("ask when needed")
    controller.apply_outcome(
        ExecutionOutcome(
            status=ExecutionOutcomeStatus.USER_INPUT_REQUIRED,
            source="policy",
            reason="need user input",
            next_action="ask_user",
            retry_allowed=False,
        )
    )
    controller.checkpoint()

    resumed = Planner(tmp_path, session_id="session_1", task_id="task_1")
    RunController(planner=resumed).resume("session_1")

    assert resumed.state is not None
    assert resumed.state.lifecycle_status == RunLifecycleStatus.WAITING_USER.value


def test_task_controller_blocks_after_max_turns(tmp_path) -> None:
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    controller = RunController(planner=planner)

    result = controller.run_loop(
        "inspect workspace",
        max_turns=1,
        run_turn=lambda _turn: None,
        on_max_turns=lambda max_turns: f"max:{max_turns}",
    )

    assert result == "max:1"
    assert planner.state is not None
    assert planner.state.lifecycle_status == RunLifecycleStatus.BLOCKED.value


def test_task_controller_run_loop_owns_turn_iteration(tmp_path) -> None:
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    controller = RunController(planner=planner)
    turns: list[int] = []

    result = controller.run_loop(
        "inspect workspace",
        max_turns=3,
        run_turn=lambda turn: turns.append(turn) or ("done" if turn == 2 else None),
        on_max_turns=lambda max_turns: f"max:{max_turns}",
    )

    assert result == "done"
    assert turns == [1, 2]
    assert planner.state is not None
    assert planner.state.lifecycle_status == RunLifecycleStatus.COMPLETED.value
