from __future__ import annotations

from pathlib import Path

from singularity.config import ApprovalMode
from singularity.config import ProductionRuntimeConfig
from singularity.kernel.graph import RuntimeFactory, RuntimeGraph
from singularity.kernel.health import RuntimeHealthChecker
from singularity.kernel.models import RuntimeComponentName, RuntimeComponentState, RunIdentity
from singularity.observability import TraceRuntime


def test_runtime_graph_initializes_components_in_declared_order(tmp_path: Path, monkeypatch) -> None:
    graph = _build_graph(tmp_path, monkeypatch, user_goal="Implement kernel")

    assert isinstance(graph, RuntimeGraph)
    assert graph.initialization_order == [
        RuntimeComponentName.CONFIGURATION,
        RuntimeComponentName.OBSERVABILITY,
        RuntimeComponentName.INTERACTION,
        RuntimeComponentName.WORKSPACE_STATE,
        RuntimeComponentName.PROJECT_INDEX,
        RuntimeComponentName.MEMORY,
        RuntimeComponentName.POLICY,
        RuntimeComponentName.SANDBOX,
        RuntimeComponentName.COMMAND,
        RuntimeComponentName.MUTATION,
        RuntimeComponentName.EDIT,
        RuntimeComponentName.TOOLS,
        RuntimeComponentName.PLUGINS,
        RuntimeComponentName.TOOL_RUNTIME,
        RuntimeComponentName.TOOL_PROTOCOL,
        RuntimeComponentName.VERIFICATION,
        RuntimeComponentName.REVIEW,
        RuntimeComponentName.EVALUATION,
        RuntimeComponentName.INSTRUCTIONS,
        RuntimeComponentName.MODEL,
        RuntimeComponentName.CONTEXT,
        RuntimeComponentName.PLANNER,
    ]
    assert graph.state(RuntimeComponentName.PLANNER) == RuntimeComponentState.READY
    assert graph.planner is not None
    assert graph.memory_runtime is not None
    assert graph.memory_runtime.session_id == graph.trace.session_id
    assert graph.components_for_health()["memory"] is graph.memory_runtime
    assert graph.edit_runtime is not None
    assert graph.review_runtime is not None
    assert graph.evaluation_runtime is not None
    assert graph.edit_runtime.review_runtime is graph.review_runtime
    assert graph.verification_runtime.review_runtime is graph.review_runtime
    assert graph.review_runtime.memory_runtime is graph.memory_runtime
    assert graph.verification_runtime.memory_runtime is graph.memory_runtime
    assert graph.evaluation_runtime.verification_runtime is graph.verification_runtime
    assert graph.evaluation_runtime.memory_runtime is graph.memory_runtime
    assert graph.evaluation_runtime.planner_runtime is graph.planner
    assert graph.components_for_health()["evaluation"] is not None
    assert graph.model_runtime is not None
    assert graph.tool_runtime is not None
    assert graph.plugin_runtime is not None
    assert graph.components_for_health()["plugins"] is graph.plugin_runtime


def test_runtime_graph_records_initialization_trace_in_declared_order(
    tmp_path: Path,
    monkeypatch,
) -> None:
    graph = _build_graph(tmp_path, monkeypatch)

    initialized = [
        event.payload["component"]
        for event in graph.trace.store.query_events()
        if event.event_type.value == "runtime.initialized"
    ]

    assert initialized == [component.value for component in graph.initialization_order]


def test_runtime_graph_exposes_stable_health_components_without_forcing_lazy_evaluation(
    tmp_path: Path,
    monkeypatch,
) -> None:
    constructed: list[dict[str, object]] = []

    class FakeEvaluationRuntime:
        def __init__(self, **kwargs) -> None:
            constructed.append(kwargs)
            self.__dict__.update(kwargs)

    monkeypatch.setattr("singularity.kernel.graph.EvaluationRuntime", FakeEvaluationRuntime)

    graph = _build_graph(tmp_path, monkeypatch)
    health_components = graph.components_for_health()

    assert set(health_components) == {component.value for component in RuntimeComponentName}
    assert health_components["evaluation"] is not None
    assert constructed == []


def test_runtime_graph_wires_planner_and_cross_runtime_dependencies(
    tmp_path: Path,
    monkeypatch,
) -> None:
    graph = _build_graph(tmp_path, monkeypatch, user_goal="Need project index and memory")

    assert graph.command_runtime.planner is graph.planner
    assert graph.mutation_runtime.planner is graph.planner
    assert graph.verification_runtime.planner is graph.planner
    assert graph.edit_runtime.planner is graph.planner
    assert graph.review_runtime.planner is graph.planner
    assert graph.planner.review_runtime is graph.review_runtime
    assert graph.tool_runtime.planner is graph.planner
    assert graph.edit_runtime.verification_runtime is graph.verification_runtime
    assert graph.edit_runtime.review_runtime is graph.review_runtime
    assert graph.verification_runtime.review_runtime is graph.review_runtime
    assert graph.verification_runtime.memory_runtime is graph.memory_runtime
    assert graph.review_runtime.memory_runtime is graph.memory_runtime
    assert graph.context_manager.model_runtime is graph.model_runtime
    assert graph.context_manager.run_id == graph.trace.run_id
    assert graph.planner.evidence.project_index_observations


def test_runtime_graph_owns_cancellation_token_targets(tmp_path: Path, monkeypatch) -> None:
    graph = _build_graph(tmp_path, monkeypatch)

    names = [name for name, _runtime in graph.cancellation_targets()]

    assert names == [
        "planner",
        "model_runtime",
        "command_runtime",
        "sandbox_runtime",
        "verification_runtime",
        "edit_runtime",
        "review_runtime",
        "tool_runtime",
        "protocol_runtime",
        "context_manager",
    ]
    assert graph._evaluation_runtime is None
    assert all(getattr(runtime, "cancellation_token") is None for _name, runtime in graph.cancellation_targets())


def test_runtime_graph_installs_cancellation_tokens_without_forcing_lazy_evaluation(
    tmp_path: Path,
    monkeypatch,
) -> None:
    constructed: list[dict[str, object]] = []

    class FakeEvaluationRuntime:
        def __init__(self, **kwargs) -> None:
            constructed.append(kwargs)
            self.__dict__.update(kwargs)

    monkeypatch.setattr("singularity.kernel.graph.EvaluationRuntime", FakeEvaluationRuntime)

    graph = _build_graph(tmp_path, monkeypatch)
    tokens: list[object] = []

    def make_token() -> object:
        token = object()
        tokens.append(token)
        return token

    graph.install_cancellation_tokens(make_token)

    assert constructed == []
    assert all(
        getattr(runtime, "cancellation_token") in tokens
        for _name, runtime in graph.cancellation_targets()
    )

    evaluation_runtime = graph.evaluation_runtime

    assert len(constructed) == 1
    assert getattr(evaluation_runtime, "cancellation_token") in tokens


def test_runtime_graph_defers_evaluation_runtime_until_used(tmp_path: Path, monkeypatch) -> None:
    constructed: list[dict[str, object]] = []

    class FakeEvaluationRuntime:
        def __init__(self, **kwargs) -> None:
            constructed.append(kwargs)
            self.__dict__.update(kwargs)

    monkeypatch.setattr("singularity.kernel.graph.EvaluationRuntime", FakeEvaluationRuntime)

    graph = _build_graph(tmp_path, monkeypatch, user_goal="Implement kernel")

    assert constructed == []
    assert graph.components_for_health()["evaluation"] is not None
    assert constructed == []

    evaluation_runtime = graph.evaluation_runtime

    assert len(constructed) == 1
    assert evaluation_runtime.verification_runtime is graph.verification_runtime
    assert evaluation_runtime.memory_runtime is graph.memory_runtime
    assert evaluation_runtime.planner_runtime is graph.planner


def test_runtime_health_reports_missing_evaluation_as_critical() -> None:
    components = {component.value: object() for component in RuntimeComponentName}
    components[RuntimeComponentName.EVALUATION.value] = None

    report = RuntimeHealthChecker().check(components)

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
) -> RuntimeGraph:
    monkeypatch.setenv("SINGULARITY_API_KEY", "test")
    monkeypatch.setenv("SINGULARITY_BASE_URL", "http://localhost/v1")
    monkeypatch.setenv("SINGULARITY_MODEL", "test-model")
    config = ProductionRuntimeConfig.from_cli(
        project_root=tmp_path,
        dry_run=True,
        approval_mode=ApprovalMode.NON_INTERACTIVE,
    )
    trace = TraceRuntime.create(tmp_path, trace_dir=tmp_path / "traces")
    return RuntimeFactory().build(
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
