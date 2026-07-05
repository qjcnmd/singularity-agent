from __future__ import annotations

from pathlib import Path

from singularity.config import ProductionConfig
from singularity.kernel.agent_kernel import AgentKernel
from singularity.kernel.graph import AgentGraph, AgentGraphBuilder
from singularity.kernel.health import ComponentHealthChecker
from singularity.kernel.lifecycle import RunLifecycleManager
from singularity.kernel.models import (
    ComponentName,
    ComponentState,
    KernelContext,
    KernelStatus,
    RunIdentity,
)
from singularity.model import ModelMessage, ModelTurnResult, ModelTurnStatus
from singularity.observability import TraceRecorder
from singularity.planner.models import TaskState
from singularity.policy.permissions import ApprovalPolicy
from singularity.session import (
    RecoveryGateDecision,
    RecoveryGateStatus,
    SessionResumeContext,
)


def test_agent_graph_initializes_components_in_declared_order(tmp_path: Path, monkeypatch) -> None:
    graph = _build_graph(tmp_path, monkeypatch, user_goal="Implement kernel")

    assert isinstance(graph, AgentGraph)
    assert graph.initialization_order == [
        ComponentName.CONFIGURATION,
        ComponentName.OBSERVABILITY,
        ComponentName.INTERACTION,
        ComponentName.WORKSPACE_STATE,
        ComponentName.PROJECT_INDEX,
        ComponentName.MEMORY,
        ComponentName.POLICY,
        ComponentName.SANDBOX,
        ComponentName.COMMAND,
        ComponentName.MUTATION,
        ComponentName.EDIT,
        ComponentName.TOOLS,
        ComponentName.PLUGINS,
        ComponentName.TOOL_EXECUTOR,
        ComponentName.TOOL_PROTOCOL,
        ComponentName.VERIFICATION,
        ComponentName.REVIEW,
        ComponentName.EVALUATION,
        ComponentName.INSTRUCTIONS,
        ComponentName.MODEL,
        ComponentName.CONTEXT,
        ComponentName.PLANNER,
    ]
    assert graph.state(ComponentName.PLANNER) == ComponentState.READY
    assert graph.planner is not None
    assert graph.memory_pipeline is not None
    assert graph.memory_pipeline.session_id == graph.trace.session_id
    assert graph.components_for_health()["memory"] is graph.memory_pipeline
    assert graph.edit_executor is not None
    assert graph.review_pipeline is not None
    assert graph.evaluation_harness is not None
    assert graph.edit_executor.review_pipeline is graph.review_pipeline
    assert graph.verification_runner.review_pipeline is graph.review_pipeline
    assert graph.review_pipeline.memory_pipeline is graph.memory_pipeline
    assert graph.verification_runner.memory_pipeline is graph.memory_pipeline
    assert graph.evaluation_harness.verification_runner is graph.verification_runner
    assert graph.evaluation_harness.memory_pipeline is graph.memory_pipeline
    assert graph.evaluation_harness.planner is graph.planner
    assert graph.components_for_health()["evaluation"] is not None
    assert graph.model_runner is not None
    assert graph.tool_executor is not None
    assert graph.plugin_manager is not None
    assert graph.components_for_health()["plugins"] is graph.plugin_manager


def test_agent_graph_records_initialization_trace_in_declared_order(
    tmp_path: Path,
    monkeypatch,
) -> None:
    graph = _build_graph(tmp_path, monkeypatch)

    initialized = [
        event.payload["component"]
        for event in graph.trace.store.query_events()
        if event.event_type.value == "component.initialized"
    ]

    assert initialized == [component.value for component in graph.initialization_order]


def test_agent_graph_exposes_stable_health_components_without_forcing_lazy_evaluation(
    tmp_path: Path,
    monkeypatch,
) -> None:
    constructed: list[dict[str, object]] = []

    class FakeEvaluationHarness:
        def __init__(self, **kwargs) -> None:
            constructed.append(kwargs)
            self.__dict__.update(kwargs)

    graph = _build_graph(
        tmp_path,
        monkeypatch,
        evaluation_harness_cls=FakeEvaluationHarness,
    )
    health_components = graph.components_for_health()

    assert set(health_components) == {component.value for component in ComponentName}
    assert health_components["evaluation"] is not None
    assert constructed == []


def test_agent_graph_wires_planner_and_cross_component_dependencies(
    tmp_path: Path,
    monkeypatch,
) -> None:
    graph = _build_graph(tmp_path, monkeypatch, user_goal="Need project index and memory")

    assert graph.command_executor.planner is graph.planner
    assert graph.mutation_manager.planner is graph.planner
    assert graph.verification_runner.planner is graph.planner
    assert graph.edit_executor.planner is graph.planner
    assert graph.review_pipeline.planner is graph.planner
    assert graph.planner.review_pipeline is graph.review_pipeline
    assert graph.tool_executor.planner is graph.planner
    assert graph.edit_executor.verification_runner is graph.verification_runner
    assert graph.edit_executor.review_pipeline is graph.review_pipeline
    assert graph.verification_runner.review_pipeline is graph.review_pipeline
    assert graph.verification_runner.memory_pipeline is graph.memory_pipeline
    assert graph.review_pipeline.memory_pipeline is graph.memory_pipeline
    assert graph.context_manager.model_runner is graph.model_runner
    assert graph.context_manager.run_id == graph.trace.run_id
    assert graph.planner.project_index is graph.project_index
    assert graph.planner.memory_pipeline is graph.memory_pipeline
    assert graph.planner.evidence.project_index_observations


def test_agent_graph_records_sandbox_capability_in_task_state(
    tmp_path: Path,
    monkeypatch,
) -> None:
    graph = _build_graph(tmp_path, monkeypatch, user_goal="Need sandbox evidence")

    snapshot = graph.planner.state.sandbox_capability
    assert snapshot["mode"] == "workspace-write"
    assert snapshot["permission"]["profile"] == "workspace-write"
    assert snapshot["permission"]["approval_policy"] == "never"
    assert snapshot["permission"]["network_access"] == "denied"
    assert snapshot["permission"]["protected_paths_enforced"] is True
    assert snapshot["enforcement_status"] in {"available", "backend_unavailable"}

    restored = TaskState.from_dict(graph.planner.state.to_dict())

    assert restored.sandbox_capability == snapshot


def test_agent_graph_owns_cancellation_token_targets(tmp_path: Path, monkeypatch) -> None:
    graph = _build_graph(tmp_path, monkeypatch)

    names = [name for name, _component in graph.cancellation_targets()]

    assert names == [
        "planner",
        "model_runner",
        "command_executor",
        "sandbox_manager",
        "verification_runner",
        "edit_executor",
        "review_pipeline",
        "tool_executor",
        "tool_protocol",
        "context_manager",
    ]
    assert graph._evaluation_harness is None
    assert all(component.cancellation_token is None for _name, component in graph.cancellation_targets())


def test_agent_graph_installs_cancellation_tokens_without_forcing_lazy_evaluation(
    tmp_path: Path,
    monkeypatch,
) -> None:
    constructed: list[dict[str, object]] = []

    class FakeEvaluationHarness:
        def __init__(self, **kwargs) -> None:
            constructed.append(kwargs)
            self.__dict__.update(kwargs)

    graph = _build_graph(
        tmp_path,
        monkeypatch,
        evaluation_harness_cls=FakeEvaluationHarness,
    )
    tokens: list[object] = []

    def make_token() -> object:
        token = object()
        tokens.append(token)
        return token

    graph.install_cancellation_tokens(make_token)

    assert constructed == []
    assert all(
        component.cancellation_token in tokens
        for _name, component in graph.cancellation_targets()
    )

    evaluation_harness = graph.evaluation_harness

    assert len(constructed) == 1
    assert evaluation_harness.cancellation_token in tokens


def test_agent_graph_defers_evaluation_harness_until_used(tmp_path: Path, monkeypatch) -> None:
    constructed: list[dict[str, object]] = []

    class FakeEvaluationHarness:
        def __init__(self, **kwargs) -> None:
            constructed.append(kwargs)
            self.__dict__.update(kwargs)

    graph = _build_graph(
        tmp_path,
        monkeypatch,
        user_goal="Implement kernel",
        evaluation_harness_cls=FakeEvaluationHarness,
    )

    assert constructed == []
    assert graph.components_for_health()["evaluation"] is not None
    assert constructed == []

    evaluation_harness = graph.evaluation_harness

    assert len(constructed) == 1
    assert evaluation_harness.verification_runner is graph.verification_runner
    assert evaluation_harness.memory_pipeline is graph.memory_pipeline
    assert evaluation_harness.planner is graph.planner


def test_graph_kernel_agentloop_model_request_chain_uses_built_components(
    tmp_path: Path,
    monkeypatch,
) -> None:
    graph = _build_graph(tmp_path, monkeypatch, user_goal="Summarize project")
    real_model_runner = graph.model_runner
    request_ids: list[str] = []

    class FakeModelRunner:
        def __init__(self, delegate) -> None:
            self.delegate = delegate

        def build_request_from_context(self, *args, **kwargs):
            request = self.delegate.build_request_from_context(*args, **kwargs)
            request_ids.append(request.request_id)
            return request

        def run_turn(self, request):
            return ModelTurnResult(
                request_id=request.request_id,
                response_id="resp_kernel_chain",
                status=ModelTurnStatus.SUCCESS,
                assistant_message=ModelMessage.assistant_text("kernel chain completed"),
            )

    fake_model_runner = FakeModelRunner(real_model_runner)
    graph.model_runner = fake_model_runner  # type: ignore[assignment]
    graph.context_manager.model_runner = fake_model_runner  # type: ignore[assignment]
    graph.planner.assess_completion = lambda mark_blocked=False: {  # type: ignore[method-assign]
        "status": "completed",
        "unmet": [],
        "criteria": {},
        "verification_contract_satisfaction": {"satisfied": True},
    }
    identity = RunIdentity.new(
        run_id=graph.trace.run_id,
        session_id=graph.trace.session_id,
        task_id=graph.trace.run_id,
    )
    lifecycle = RunLifecycleManager(identity=identity, trace=graph.trace)
    run = lifecycle.create_run("Summarize project")
    session = lifecycle.start_session()
    context = KernelContext(
        project_root=tmp_path,
        identity=identity,
        run=run,
        session=session,
        status=KernelStatus.READY,
        workspace_lock_status="acquired",
    )

    class Lock:
        released = False

        def release_lock(self) -> None:
            self.released = True

    result = AgentKernel(
        context=context,
        graph=graph,
        lifecycle=lifecycle,
        workspace_lock=Lock(),
    ).run_task("Summarize project")

    assert result.final_answer == "kernel chain completed"
    assert request_ids
    assert request_ids[0].startswith("model_req_")
    events = graph.trace.store.query_events()
    assert any(event.event_type.value == "final_report.completed" for event in events)


def test_agent_kernel_blocks_before_model_when_recovery_gate_requires_review(
    tmp_path: Path,
    monkeypatch,
) -> None:
    decision = RecoveryGateDecision(
        session_id="session_gate",
        mode="resume",
        status=RecoveryGateStatus.NEEDS_REVIEW,
        can_call_model=False,
        blockers=["external_user_change"],
        warnings=[],
        next_action="run sg session show session_gate --timeline",
        resume_context=SessionResumeContext(
            session_id="session_gate",
            workspace={"external_changes": ["README.md"]},
        ),
    )
    graph = _build_graph(tmp_path, monkeypatch, user_goal="Recover session")
    graph.recovery_gate_decision = decision
    identity = RunIdentity.new(
        run_id=graph.trace.run_id,
        session_id="session_gate",
        task_id="task_gate",
    )
    lifecycle = RunLifecycleManager(identity=identity, trace=graph.trace)
    context = KernelContext(
        project_root=tmp_path,
        identity=identity,
        run=lifecycle.create_run("Recover session"),
        session=lifecycle.start_session(),
        status=KernelStatus.READY,
        workspace_lock_status="acquired",
        recovery_gate_decision=decision.to_dict(),
    )

    class Lock:
        def release_lock(self) -> None:
            pass

    result = AgentKernel(
        context=context,
        graph=graph,
        lifecycle=lifecycle,
        workspace_lock=Lock(),
    ).run_task("Recover session")

    assert result.status.value == "blocked"
    assert result.final_report.recovery_gate_summary["status"] == "needs_review"
    assert result.final_report.recovery_gate_summary["can_call_model"] is False
    assert "external_user_change" in result.final_answer
    assert not [
        event
        for event in graph.trace.store.query_events()
        if event.event_type.value == "model_request.created"
    ]


def test_component_health_reports_missing_evaluation_as_critical() -> None:
    components = {component.value: object() for component in ComponentName}
    components[ComponentName.EVALUATION.value] = None

    report = ComponentHealthChecker().check(components)

    assert report.ok is False
    assert report.summary["evaluation"] == "missing"
    assert {
        "component": "evaluation",
        "status": "missing",
        "critical": True,
    } in report.diagnostics


def _build_graph(
    tmp_path: Path,
    monkeypatch,
    *,
    user_goal: str = "Implement kernel",
    evaluation_harness_cls=None,
) -> AgentGraph:
    monkeypatch.setenv("SINGULARITY_API_KEY", "test")
    monkeypatch.setenv("SINGULARITY_BASE_URL", "http://localhost/v1")
    monkeypatch.setenv("SINGULARITY_MODEL", "test-model")
    config = ProductionConfig.from_cli(
        project_root=tmp_path,
        dry_run=True,
        approval_policy=ApprovalPolicy.NEVER,
    )
    trace = TraceRecorder.create(tmp_path, trace_dir=tmp_path / "traces")
    builder = AgentGraphBuilder(
        evaluation_harness_factory_builder=_evaluation_harness_factory_builder(
            evaluation_harness_cls
        ),
    )
    return builder.build(
        project_root=tmp_path,
        config=config,
        trace=trace,
        identity=RunIdentity.new(
            run_id=trace.run_id,
            session_id=trace.session_id,
            task_id=trace.run_id,
        ),
        user_goal=user_goal,
    )


def _evaluation_harness_factory_builder(evaluation_harness_cls=None):
    from singularity.evaluation.factory import build_evaluation_harness_factory

    if evaluation_harness_cls is None:
        return build_evaluation_harness_factory

    def build_factory(**kwargs):
        def build_harness():
            return evaluation_harness_cls(
                project_root=kwargs["project_root"],
                trace_recorder=kwargs["trace"],
                verification_runner=kwargs["verification_review"].verification_runner,
                memory_pipeline=kwargs["infra"].memory_pipeline,
                planner=kwargs["planner"],
                tool_executor=kwargs["tool_protocol"].tool_executor,
                command_executor=kwargs["execution_core"].command_executor,
                mutation_manager=kwargs["execution_core"].mutation_manager,
            )

        return build_harness

    return build_factory
