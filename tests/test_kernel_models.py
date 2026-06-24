from __future__ import annotations

from pathlib import Path

from singularity.kernel.models import (
    AgentRun,
    AgentSession,
    KernelContext,
    KernelStatus,
    LifecycleEvent,
    RunIdentity,
    RunStatus,
    ComponentName,
    ComponentState,
    SessionStatus,
    ShutdownReason,
)


def test_run_identity_creates_linked_run_session_and_task_ids() -> None:
    identity = RunIdentity.new()

    assert identity.run_id.startswith("run_")
    assert identity.session_id.startswith("session_")
    assert identity.task_id.startswith("task_")
    assert identity.to_dict()["run_id"] == identity.run_id


def test_kernel_context_serializes_safe_component_state(tmp_path: Path) -> None:
    identity = RunIdentity.new(run_id="run_1", session_id="session_1", task_id="task_1")
    run = AgentRun(identity=identity, user_goal="Implement kernel")
    session = AgentSession(identity=identity)
    context = KernelContext(
        project_root=tmp_path,
        identity=identity,
        run=run,
        session=session,
        status=KernelStatus.READY,
        components={
            ComponentName.PLANNER: ComponentState.READY,
            ComponentName.MODEL: ComponentState.INITIALIZED,
        },
        workspace_lock_status="acquired",
    )

    payload = context.to_dict()

    assert payload["project_root"] == str(tmp_path)
    assert payload["status"] == "ready"
    assert payload["run"]["status"] == RunStatus.CREATED.value
    assert payload["session"]["status"] == SessionStatus.CREATED.value
    assert payload["components"]["planner"] == "ready"
    assert "api_key" not in str(payload).lower()


def test_lifecycle_event_contains_identity_and_payload() -> None:
    identity = RunIdentity.new(run_id="run_1", session_id="session_1", task_id="task_1")

    event = LifecycleEvent.from_identity(
        "lifecycle.run.started",
        identity,
        payload={"shutdown_reason": ShutdownReason.NORMAL.value},
    )

    assert event.event_type == "lifecycle.run.started"
    assert event.run_id == "run_1"
    assert event.session_id == "session_1"
    assert event.task_id == "task_1"
    assert event.payload == {"shutdown_reason": "normal"}
    assert event.to_dict()["payload"]["shutdown_reason"] == "normal"
