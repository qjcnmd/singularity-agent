from __future__ import annotations

import pytest

from singularity.kernel.lifecycle import RunLifecycleManager
from singularity.kernel.models import RunStatus, SessionStatus


class RecordingTrace:
    def __init__(self) -> None:
        self.events: list[tuple[str, dict]] = []

    def record(self, event: str, data: dict) -> None:
        self.events.append((event, data))


def test_lifecycle_manager_records_valid_run_session_and_task_flow() -> None:
    trace = RecordingTrace()
    lifecycle = RunLifecycleManager(trace=trace)

    run = lifecycle.create_run("Build kernel")
    session = lifecycle.start_session()
    task = lifecycle.start_task("Build kernel")
    lifecycle.mark_completed()

    assert run.status == RunStatus.COMPLETED
    assert session.status == SessionStatus.CLOSED
    assert task.status == RunStatus.COMPLETED
    assert [event.event_type for event in lifecycle.events] == [
        "lifecycle.run.started",
        "lifecycle.session.started",
        "lifecycle.task.started",
        "lifecycle.run.completed",
    ]
    assert [event for event, _ in trace.events] == ["lifecycle"] * 4


def test_lifecycle_manager_rejects_task_before_session() -> None:
    lifecycle = RunLifecycleManager()
    lifecycle.create_run("Build kernel")

    with pytest.raises(ValueError):
        lifecycle.start_task("Build kernel")
