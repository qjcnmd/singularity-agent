from __future__ import annotations

from pathlib import Path
from types import SimpleNamespace

from singularity.agent_host import AgentHost, RunEvent, ToolCallEvent
from singularity.config import ProductionConfig
from singularity.kernel.models import RunIdentity, RunStatus
from singularity.observability import TraceRecorder
from singularity.observability.models import TraceArtifactKind
from singularity.policy import ApprovalGrant, ApprovalScope
from singularity.policy.permissions import ApprovalPolicy


def test_agent_host_projects_trace_events_with_sequence_and_replay(tmp_path: Path) -> None:
    trace = TraceRecorder.create(tmp_path, run_id="run_1", session_id="session_1")
    trace.emit("lifecycle.run.started", component="test", summary="started", ids={"task_id": "task_1"})
    trace.emit("tool_protocol.call_completed", component="tool_protocol", summary="read_file completed")
    host = AgentHost(tmp_path)

    events = host.events("run_1")
    replay = host.events("run_1", after_sequence=0)

    assert [event.sequence for event in events] == [0, 1]
    assert events[0].schema_version == "1.0"
    assert events[0].run_id == "run_1"
    assert events[0].redaction_applied is True
    assert [event.sequence for event in replay] == [1]
    assert "path" not in events[0].to_dict()


def test_agent_host_reads_artifacts_by_opaque_ref(tmp_path: Path) -> None:
    trace = TraceRecorder.create(tmp_path, run_id="run_artifact", session_id="session_artifact")
    artifact = trace.write_artifact(kind=TraceArtifactKind.REPORT, text="report body")
    host = AgentHost(tmp_path)

    assert host.read_artifact("run_artifact", artifact.artifact_id) == b"report body"


def test_agent_host_start_run_wraps_kernel_without_exposing_agent_graph(tmp_path: Path) -> None:
    trace = TraceRecorder.create(tmp_path, run_id="run_kernel", session_id="session_kernel")
    approval_gate = _ApprovalGate()
    kernel = _Kernel(trace=trace, approval_gate=approval_gate)
    host = AgentHost(tmp_path, bootstrap_factory=lambda **_kwargs: _Bootstrap(kernel))
    config = ProductionConfig.from_cli(
        project_root=tmp_path,
        approval_policy=ApprovalPolicy.NEVER,
        dry_run=True,
    )

    result = host.start_run("finish task", config=config)
    snapshot = result.snapshot.to_dict()

    assert result.status == "completed"
    assert result.final_answer == "done"
    assert snapshot["run_id"] == "run_kernel"
    assert snapshot["event_count"] == 1
    assert "graph" not in snapshot
    assert "policy_engine" not in snapshot


def test_agent_host_resume_run_preserves_explicit_config_and_sets_resume_session(tmp_path: Path) -> None:
    trace = TraceRecorder.create(tmp_path, run_id="run_resume", session_id="session_new")
    approval_gate = _ApprovalGate()
    kernel = _Kernel(trace=trace, approval_gate=approval_gate)
    bootstrap_factory = _BootstrapFactory(kernel)
    host = AgentHost(tmp_path, bootstrap_factory=bootstrap_factory)
    config = ProductionConfig.from_cli(
        project_root=tmp_path,
        approval_policy=ApprovalPolicy.NEVER,
        dry_run=True,
    )

    host.resume_run("session_previous", "continue task", config=config)

    assert bootstrap_factory.configs[0].resume_session == "session_previous"
    assert bootstrap_factory.configs[0].approval_policy == ApprovalPolicy.NEVER
    assert bootstrap_factory.configs[0].dry_run is True


def test_agent_host_registers_approval_grants_through_approval_gate(tmp_path: Path) -> None:
    trace = TraceRecorder.create(tmp_path, run_id="run_approval", session_id="session_approval")
    approval_gate = _ApprovalGate()
    kernel = _Kernel(trace=trace, approval_gate=approval_gate)
    host = AgentHost(tmp_path, bootstrap_factory=lambda **_kwargs: _Bootstrap(kernel))
    config = ProductionConfig.from_cli(
        project_root=tmp_path,
        approval_policy=ApprovalPolicy.NEVER,
        dry_run=True,
    )
    host.start_run("wait for approval", config=config)
    grant = ApprovalGrant(
        decision_id="policy_dec_1",
        request_id="policy_req_1",
        approved_by="test",
        session_id="session_approval",
        scope=ApprovalScope(capabilities=["READ_WORKSPACE"]),
        reason="approved by test",
    )

    event = host.submit_approval("run_approval", grant.to_dict())

    assert approval_gate.registered == [grant.grant_id]
    assert event.to_dict()["status"] == "granted"
    assert host.events("run_approval")[-1].event_type == "approval.granted"


def test_agent_host_cancel_run_reports_cancel_requested(tmp_path: Path) -> None:
    trace = TraceRecorder.create(tmp_path, run_id="run_cancel", session_id="session_cancel")
    approval_gate = _ApprovalGate()
    kernel = _Kernel(trace=trace, approval_gate=approval_gate)
    host = AgentHost(tmp_path, bootstrap_factory=lambda **_kwargs: _Bootstrap(kernel))
    config = ProductionConfig.from_cli(
        project_root=tmp_path,
        approval_policy=ApprovalPolicy.NEVER,
        dry_run=True,
    )
    host.start_run("wait for cancel", config=config)

    snapshot = host.cancel_run("run_cancel")

    assert kernel.cancelled
    assert snapshot.status == "cancel_requested"


def test_tool_call_event_projects_from_run_event() -> None:
    event = RunEvent(
        event_id="event_1",
        event_type="tool_protocol.call_completed",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        action_id="call_1",
        component="tool_protocol",
        severity="info",
        timestamp="2026-01-01T00:00:00+00:00",
        sequence=1,
        summary="tool completed",
        payload={
            "tool_call_id": "call_1",
            "tool_name": "read_file",
            "argument_digest": "abc",
        },
    )

    projected = ToolCallEvent.from_run_event(event)

    assert projected.tool_call_id == "call_1"
    assert projected.tool_name == "read_file"
    assert projected.phase == "succeeded"
    assert projected.argument_digest == "abc"


class _Bootstrap:
    def __init__(self, kernel: _Kernel) -> None:
        self.kernel = kernel

    def boot(self, _goal: str) -> _Kernel:
        return self.kernel


class _BootstrapFactory:
    def __init__(self, kernel: _Kernel) -> None:
        self.kernel = kernel
        self.configs: list[ProductionConfig] = []

    def __call__(self, **kwargs) -> _Bootstrap:
        self.configs.append(kwargs["config"])
        return _Bootstrap(self.kernel)


class _Kernel:
    def __init__(self, *, trace: TraceRecorder, approval_gate: _ApprovalGate) -> None:
        identity = RunIdentity(run_id=trace.run_id, session_id=trace.session_id, task_id="task_kernel")
        self.context = SimpleNamespace(identity=identity)
        self.graph = SimpleNamespace(trace=trace, approval_gate=approval_gate)
        self.cancelled = False
        self.closed = False

    def run_task(self, _goal: str) -> SimpleNamespace:
        self.graph.trace.emit(
            "lifecycle.run.started",
            component="kernel",
            summary="run started",
            ids={"run_id": self.context.identity.run_id, "task_id": self.context.identity.task_id},
        )
        return SimpleNamespace(
            status=RunStatus.COMPLETED,
            final_answer="done",
            final_report=SimpleNamespace(to_dict=lambda: {"status": "completed"}),
        )

    def cancel(self, _reason, _message: str) -> None:
        self.cancelled = True

    def close_resources(self) -> None:
        self.closed = True


class _ApprovalGate:
    def __init__(self) -> None:
        self.registered: list[str] = []

    def register_grant(self, grant: ApprovalGrant) -> None:
        self.registered.append(grant.grant_id)
