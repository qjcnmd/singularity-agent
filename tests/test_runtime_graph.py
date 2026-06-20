from __future__ import annotations

from pathlib import Path

from miniharness.config import ProductionRuntimeConfig
from miniharness.kernel.graph import RuntimeFactory, RuntimeGraph
from miniharness.kernel.health import RuntimeHealthChecker
from miniharness.kernel.models import RuntimeComponentName, RuntimeComponentState, RunIdentity
from miniharness.observability import TraceRuntime


def test_runtime_graph_initializes_components_in_declared_order(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.setenv("MINIHARNESS_API_KEY", "test")
    monkeypatch.setenv("MINIHARNESS_BASE_URL", "http://localhost/v1")
    monkeypatch.setenv("MINIHARNESS_MODEL", "test-model")
    config = ProductionRuntimeConfig.from_cli(project_root=tmp_path, dry_run=True)
    trace = TraceRuntime.create(tmp_path, trace_dir=tmp_path / "traces")

    graph = RuntimeFactory().build(
        project_root=tmp_path,
        config=config,
        trace=trace,
        identity=RunIdentity.new(run_id=trace.run_id, session_id=trace.session_id, task_id=trace.run_id),
        user_goal="Implement kernel",
    )

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
    assert graph.memory_runtime.session_id == trace.session_id
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
    assert graph.components_for_health()["evaluation"] is graph.evaluation_runtime
    assert graph.model_runtime is not None
    assert graph.tool_runtime is not None


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
