from __future__ import annotations

from pathlib import Path
from types import SimpleNamespace

from singularity.agent_host import AgentHost
from singularity.config import ProductionConfig
from singularity.kernel.models import RunIdentity, RunStatus
from singularity.observability import TraceRecorder
from singularity.policy.permissions import ApprovalPolicy


def test_agent_host_cancel_run_requests_kernel_cancel(tmp_path: Path) -> None:
    trace = TraceRecorder.create(tmp_path, run_id="run_cancel", session_id="session_cancel")
    kernel = _Kernel(trace)
    host = AgentHost(tmp_path, bootstrap_factory=lambda **_kwargs: _Bootstrap(kernel))
    config = ProductionConfig.from_cli(
        project_root=tmp_path,
        approval_policy=ApprovalPolicy.NEVER,
        dry_run=True,
    )
    host.start_run("finish task", config=config)

    snapshot = host.cancel_run("run_cancel")

    assert kernel.cancelled
    assert snapshot.status == "cancel_requested"
    assert snapshot.run_id == "run_cancel"


class _Bootstrap:
    def __init__(self, kernel: _Kernel) -> None:
        self.kernel = kernel

    def boot(self, _goal: str) -> _Kernel:
        return self.kernel


class _Kernel:
    def __init__(self, trace: TraceRecorder) -> None:
        identity = RunIdentity(
            run_id=trace.run_id,
            session_id=trace.session_id,
            task_id="task_cancel",
        )
        self.context = SimpleNamespace(identity=identity)
        self.graph = SimpleNamespace(trace=trace)
        self.cancelled = False

    def run_task(self, _goal: str) -> SimpleNamespace:
        self.graph.trace.emit(
            "lifecycle.run.started",
            component="kernel",
            summary="run started",
            ids={
                "run_id": self.context.identity.run_id,
                "task_id": self.context.identity.task_id,
            },
        )
        return SimpleNamespace(
            status=RunStatus.COMPLETED,
            final_answer="done",
            final_report=SimpleNamespace(to_dict=lambda: {"status": "completed"}),
        )

    def cancel(self, _reason, _message: str) -> None:
        self.cancelled = True
