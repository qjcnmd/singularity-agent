from __future__ import annotations

from pathlib import Path

import pytest

from miniharness.config import ProductionRuntimeConfig
from miniharness.agent import MiniAgentRunResult, MiniAgentRunStatus
from miniharness.kernel import CancellationError
from miniharness.kernel.cancellation import CancellationManager
from miniharness.kernel.finalization import KernelFinalizer
from miniharness.kernel.lifecycle import RunLifecycleManager
from miniharness.kernel.models import (
    AgentRun,
    KernelContext,
    KernelStatus,
    CancellationReason,
    RunIdentity,
    RunStatus,
    ShutdownReason,
)
from miniharness.kernel.runtime import AgentKernel
from miniharness.kernel.shutdown import ShutdownSummary
from miniharness.workspace_state import WorkspaceHealthReport, WorkspaceHealthStatus


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
        runtime_health_summary={"planner": "ok"},
        shutdown_summary=ShutdownSummary(ShutdownReason.NORMAL, "completed", []),
        recovery_summary={"recovered": False},
        lifecycle_summary={"events": 3},
    )

    payload = report.to_dict()
    assert payload["run_id"] == "run_1"
    assert payload["session_id"] == "session_1"
    assert payload["task_id"] == "task_1"
    assert payload["kernel_status"] == "finalized"
    assert payload["runtime_health_summary"] == {"planner": "ok"}
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

    monkeypatch.setattr("miniharness.kernel.runtime.MiniAgent.run", fail_run)

    with pytest.raises(RuntimeError, match="planner failed"):
        kernel.run_task("Build kernel")

    assert kernel.context.status == KernelStatus.FINALIZED
    assert trace.has_event("finalization.completed")
    report = kernel.final_report()
    assert report.shutdown_reason == ShutdownReason.ERROR.value
    assert report.diagnostics_count == 1


def test_agent_kernel_maps_blocked_agent_result_to_failed_run(
    tmp_path: Path,
    monkeypatch,
) -> None:
    kernel, _trace = _build_kernel(tmp_path)

    def blocked_run(*args, **kwargs):
        return MiniAgentRunResult(
            status=MiniAgentRunStatus.BLOCKED,
            final_answer="Planner blocked finalization",
            turn=1,
            error_code="completion_blocked",
        )

    monkeypatch.setattr("miniharness.kernel.runtime.MiniAgent.run", blocked_run)

    result = kernel.run_task("Build kernel")

    assert result.status == RunStatus.FAILED
    assert kernel.context.run.status == RunStatus.FAILED
    assert kernel.final_report().shutdown_reason == ShutdownReason.ERROR.value


def test_agent_kernel_maps_max_turns_to_failed_run(
    tmp_path: Path,
    monkeypatch,
) -> None:
    kernel, _trace = _build_kernel(tmp_path)

    def max_turns_run(*args, **kwargs):
        return MiniAgentRunResult(
            status=MiniAgentRunStatus.MAX_TURNS_EXCEEDED,
            final_answer="Stopped after max_turns=1",
            turn=1,
            error_code="max_turns_exceeded",
        )

    monkeypatch.setattr("miniharness.kernel.runtime.MiniAgent.run", max_turns_run)

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


def test_agent_kernel_shutdown_rejects_late_runtime_actions(tmp_path: Path) -> None:
    kernel, _trace = _build_kernel(tmp_path)

    kernel.shutdown(ShutdownReason.ERROR)

    for runtime in (
        kernel.graph.planner,
        kernel.graph.model_runtime,
        kernel.graph.command_runtime,
        kernel.graph.sandbox_runtime,
        kernel.graph.verification_runtime,
    ):
        with pytest.raises(CancellationError):
            runtime.cancellation_token.throw_if_cancelled()


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
    def record_external_changes(self) -> None:
        pass

    def get_workspace_health(self) -> WorkspaceHealthReport:
        return WorkspaceHealthReport(status=WorkspaceHealthStatus.CLEAN)


class _Planner:
    final_report = None
    state = None

    def interrupt(self, reason: str) -> None:
        self.interrupt_reason = reason


class _Runtime:
    pass


class _Graph:
    def __init__(self, tmp_path: Path, trace: _Trace) -> None:
        self.config = ProductionRuntimeConfig.from_cli(project_root=tmp_path, dry_run=True)
        self.trace = trace
        self.workspace_state = _WorkspaceState()
        self.planner = _Planner()
        self.model_runtime = _Runtime()
        self.command_runtime = _Runtime()
        self.sandbox_runtime = _Runtime()
        self.verification_runtime = _Runtime()
        self.review_runtime = _Runtime()
        self.mutation_runtime = _Runtime()
        self.tools = _Runtime()
        self.policy_runtime = _Runtime()
        self.tool_runtime = _Runtime()
        self.protocol_runtime = _Runtime()
        self.instruction_runtime = _Runtime()
        self.context_manager = _Runtime()

    def cancellation_targets(self) -> list[tuple[str, object]]:
        return [
            ("planner", self.planner),
            ("model_runtime", self.model_runtime),
            ("command_runtime", self.command_runtime),
            ("sandbox_runtime", self.sandbox_runtime),
            ("verification_runtime", self.verification_runtime),
            ("review_runtime", self.review_runtime),
            ("tool_runtime", self.tool_runtime),
            ("protocol_runtime", self.protocol_runtime),
            ("context_manager", self.context_manager),
        ]

    def install_cancellation_tokens(self, token_factory) -> None:
        for _name, runtime in self.cancellation_targets():
            setattr(runtime, "cancellation_token", token_factory())


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
