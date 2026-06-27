from __future__ import annotations

from pathlib import Path

from singularity.config import ProductionConfig
from singularity.policy.permissions import ApprovalPolicy
from singularity.kernel.graph import AgentGraphBuilder, AgentGraph
from singularity.kernel.health import ComponentHealthChecker
from singularity.kernel.models import ComponentName, ComponentState, RunIdentity
from singularity.observability import TraceRecorder
from singularity.planner.models import TaskState


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

    monkeypatch.setattr("singularity.kernel.graph.EvaluationHarness", FakeEvaluationHarness)

    graph = _build_graph(tmp_path, monkeypatch)
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
    assert all(getattr(component, "cancellation_token") is None for _name, component in graph.cancellation_targets())


def test_agent_graph_installs_cancellation_tokens_without_forcing_lazy_evaluation(
    tmp_path: Path,
    monkeypatch,
) -> None:
    constructed: list[dict[str, object]] = []

    class FakeEvaluationHarness:
        def __init__(self, **kwargs) -> None:
            constructed.append(kwargs)
            self.__dict__.update(kwargs)

    monkeypatch.setattr("singularity.kernel.graph.EvaluationHarness", FakeEvaluationHarness)

    graph = _build_graph(tmp_path, monkeypatch)
    tokens: list[object] = []

    def make_token() -> object:
        token = object()
        tokens.append(token)
        return token

    graph.install_cancellation_tokens(make_token)

    assert constructed == []
    assert all(
        getattr(component, "cancellation_token") in tokens
        for _name, component in graph.cancellation_targets()
    )

    evaluation_harness = graph.evaluation_harness

    assert len(constructed) == 1
    assert getattr(evaluation_harness, "cancellation_token") in tokens


def test_agent_graph_defers_evaluation_harness_until_used(tmp_path: Path, monkeypatch) -> None:
    constructed: list[dict[str, object]] = []

    class FakeEvaluationHarness:
        def __init__(self, **kwargs) -> None:
            constructed.append(kwargs)
            self.__dict__.update(kwargs)

    monkeypatch.setattr("singularity.kernel.graph.EvaluationHarness", FakeEvaluationHarness)

    graph = _build_graph(tmp_path, monkeypatch, user_goal="Implement kernel")

    assert constructed == []
    assert graph.components_for_health()["evaluation"] is not None
    assert constructed == []

    evaluation_harness = graph.evaluation_harness

    assert len(constructed) == 1
    assert evaluation_harness.verification_runner is graph.verification_runner
    assert evaluation_harness.memory_pipeline is graph.memory_pipeline
    assert evaluation_harness.planner is graph.planner


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
    return AgentGraphBuilder().build(
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
