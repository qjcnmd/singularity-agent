from __future__ import annotations

from pathlib import Path

import pytest

from singularity.config import ProductionConfig
from singularity.agent_loop import AgentLoopResult, AgentLoopStatus
from singularity.kernel import CancellationError
from singularity.kernel.cancellation import CancellationManager
from singularity.kernel.finalization import KernelFinalizer
from singularity.kernel.lifecycle import RunLifecycleManager
from singularity.kernel.models import (
    AgentRun,
    KernelContext,
    KernelStatus,
    CancellationReason,
    RunIdentity,
    RunStatus,
    ShutdownReason,
)
from singularity.kernel.agent_kernel import AgentKernel
from singularity.kernel.shutdown import ShutdownSummary
from singularity.workspace_state import WorkspaceHealthReport, WorkspaceHealthStatus


def test_kernel_finalizer_builds_safe_final_report(tmp_path) -> None:
    identity = RunIdentity.new(run_id="run_1", session_id="session_1", task_id="task_1")
    context = KernelContext(
        project_root=tmp_path,
        identity=identity,
        run=AgentRun(identity=identity, user_goal="Build kernel"),
        status=KernelStatus.SHUTTING_DOWN,
        diagnostics=[{"message": "ok"}],
        workspace_lock_status="released",
    )
    finalizer = KernelFinalizer()

    report = finalizer.finalize(
        context=context,
        planner_report={"status": "completed", "api_key": "secret-value"},
        component_health_summary={"planner": "ok"},
        shutdown_summary=ShutdownSummary(ShutdownReason.NORMAL, "completed", []),
        recovery_summary={"recovered": False},
        lifecycle_summary={"events": 3},
    )

    payload = report.to_dict()
    assert payload["run_id"] == "run_1"
    assert payload["session_id"] == "session_1"
    assert payload["task_id"] == "task_1"
    assert payload["kernel_status"] == "finalized"
    assert payload["component_health_summary"] == {"planner": "ok"}
    assert payload["shutdown_summary"]["cleanup_status"] == "completed"
    assert payload["diagnostics_count"] == 1
    assert "secret-value" not in str(payload)


def test_agent_kernel_finalizes_cancelled_run_before_raising(tmp_path: Path) -> None:
    kernel, trace = _build_kernel(tmp_path)
    kernel.cancellation.cancel(CancellationReason.USER_INTERRUPTED, "stop now")

    with pytest.raises(CancellationError):
        kernel.run_task("Build kernel")

    assert kernel.context.status == KernelStatus.FINALIZED
    assert trace.has_event("finalization.completed")
    assert kernel.final_report().shutdown_reason == ShutdownReason.CANCELLED.value


def test_agent_kernel_finalizes_failed_run_before_reraising(
    tmp_path: Path,
    monkeypatch,
) -> None:
    kernel, trace = _build_kernel(tmp_path)

    def fail_run(*args, **kwargs):
        raise RuntimeError("planner failed")

    monkeypatch.setattr("singularity.kernel.agent_kernel.AgentLoop.run", fail_run)

    with pytest.raises(RuntimeError, match="planner failed"):
        kernel.run_task("Build kernel")

    assert kernel.context.status == KernelStatus.FINALIZED
    assert trace.has_event("finalization.completed")
    report = kernel.final_report()
    assert report.shutdown_reason == ShutdownReason.ERROR.value
    assert report.diagnostics_count == 1


def test_agent_kernel_preserves_blocked_agent_result(
    tmp_path: Path,
    monkeypatch,
) -> None:
    kernel, _trace = _build_kernel(tmp_path)

    def blocked_run(*args, **kwargs):
        return AgentLoopResult(
            status=AgentLoopStatus.BLOCKED,
            final_answer="Planner blocked finalization",
            turn=1,
            error_code="completion_blocked",
        )

    monkeypatch.setattr("singularity.kernel.agent_kernel.AgentLoop.run", blocked_run)

    result = kernel.run_task("Build kernel")

    assert result.status == RunStatus.BLOCKED
    assert kernel.context.run.status == RunStatus.BLOCKED
    assert kernel.final_report().shutdown_reason == ShutdownReason.BLOCKED.value


def test_agent_kernel_maps_max_turns_to_failed_run(
    tmp_path: Path,
    monkeypatch,
) -> None:
    kernel, _trace = _build_kernel(tmp_path)

    def max_turns_run(*args, **kwargs):
        return AgentLoopResult(
            status=AgentLoopStatus.MAX_TURNS_EXCEEDED,
            final_answer="Stopped after max_turns=1",
            turn=1,
            error_code="max_turns_exceeded",
        )

    monkeypatch.setattr("singularity.kernel.agent_kernel.AgentLoop.run", max_turns_run)

    result = kernel.run_task("Build kernel")

    assert result.status == RunStatus.FAILED
    assert kernel.context.run.status == RunStatus.FAILED
    assert kernel.final_report().shutdown_reason == ShutdownReason.ERROR.value


def test_agent_kernel_shutdown_writes_final_report_during_shutdown_step(tmp_path: Path) -> None:
    kernel, _trace = _build_kernel(tmp_path)

    summary = kernel.shutdown(ShutdownReason.NORMAL)

    write_report_step = next(step for step in summary.steps if step["step"] == "write_report")
    assert write_report_step["status"] == "completed"
    assert kernel.context.status == KernelStatus.FINALIZED
    assert kernel.final_report().shutdown_reason == ShutdownReason.NORMAL.value
    assert kernel.workspace_lock.released is True


def test_agent_kernel_shutdown_rejects_late_component_actions(tmp_path: Path) -> None:
    kernel, _trace = _build_kernel(tmp_path)

    kernel.shutdown(ShutdownReason.ERROR)

    for component in (
        kernel.graph.planner,
        kernel.graph.model_runner,
        kernel.graph.command_executor,
        kernel.graph.sandbox_manager,
        kernel.graph.verification_runner,
    ):
        with pytest.raises(CancellationError):
            component.cancellation_token.throw_if_cancelled()


def test_agent_kernel_close_resources_closes_stateful_components(tmp_path: Path) -> None:
    kernel, _trace = _build_kernel(tmp_path)

    kernel.shutdown(ShutdownReason.NORMAL)
    kernel.close_resources()

    assert kernel.graph.workspace_state.closed is True
    assert kernel.graph.workspace_state.closed_session_status == "closed"
    assert kernel.graph.context_manager.closed is True
    assert kernel.graph.tool_protocol.closed is True


class _Trace:
    def __init__(self, tmp_path: Path) -> None:
        self.events: list[tuple[str, dict]] = []

        class Store:
            run_dir = tmp_path / "traces" / "run_1"

        self.store = Store()

    def record(self, event: str, data: dict) -> None:
        self.events.append((event, data))

    def has_event(self, event: str) -> bool:
        return any(recorded == event for recorded, _ in self.events)


class _WorkspaceState:
    closed = False
    closed_session_status = None

    def record_external_changes(self) -> None:
        pass

    def get_workspace_health(self) -> WorkspaceHealthReport:
        return WorkspaceHealthReport(status=WorkspaceHealthStatus.CLEAN)

    def close_session(self, *, status: str = "closed") -> None:
        self.closed_session_status = status

    def close(self) -> None:
        self.closed = True


class _Planner:
    final_report = None
    state = None

    def interrupt(self, reason: str) -> None:
        self.interrupt_reason = reason


class _Component:
    pass


class _ClosableComponent:
    def __init__(self) -> None:
        self.closed = False

    def close(self) -> None:
        self.closed = True


class _Graph:
    def __init__(self, tmp_path: Path, trace: _Trace) -> None:
        self.config = ProductionConfig.from_cli(project_root=tmp_path, dry_run=True)
        self.trace = trace
        self.workspace_state = _WorkspaceState()
        self.planner = _Planner()
        self.model_runner = _Component()
        self.command_executor = _Component()
        self.sandbox_manager = _Component()
        self.verification_runner = _Component()
        self.review_pipeline = _Component()
        self.mutation_manager = _Component()
        self.tools = _Component()
        self.policy_engine = _Component()
        self.tool_executor = _Component()
        self.tool_protocol = _ClosableComponent()
        self.prompt_assembly = _Component()
        self.context_manager = _ClosableComponent()

    def cancellation_targets(self) -> list[tuple[str, object]]:
        return [
            ("planner", self.planner),
            ("model_runner", self.model_runner),
            ("command_executor", self.command_executor),
            ("sandbox_manager", self.sandbox_manager),
            ("verification_runner", self.verification_runner),
            ("review_pipeline", self.review_pipeline),
            ("tool_executor", self.tool_executor),
            ("tool_protocol", self.tool_protocol),
            ("context_manager", self.context_manager),
        ]

    def install_cancellation_tokens(self, token_factory) -> None:
        for _name, component in self.cancellation_targets():
            setattr(component, "cancellation_token", token_factory())


class _Lock:
    def __init__(self) -> None:
        self.released = False

    def release_lock(self) -> None:
        self.released = True


def _build_kernel(tmp_path: Path) -> tuple[AgentKernel, _Trace]:
    identity = RunIdentity.new(run_id="run_1", session_id="session_1", task_id="task_1")
    trace = _Trace(tmp_path)
    lifecycle = RunLifecycleManager(identity=identity, trace=trace)
    run = lifecycle.create_run("Build kernel")
    session = lifecycle.start_session()
    context = KernelContext(
        project_root=tmp_path,
        identity=identity,
        run=run,
        session=session,
        status=KernelStatus.READY,
        workspace_lock_status="acquired",
    )
    kernel = AgentKernel(
        context=context,
        graph=_Graph(tmp_path, trace),
        lifecycle=lifecycle,
        workspace_lock=_Lock(),
        cancellation=CancellationManager(),
    )
    return kernel, trace
