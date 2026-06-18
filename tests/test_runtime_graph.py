from __future__ import annotations

from pathlib import Path

from miniharness.config import ProductionRuntimeConfig
from miniharness.kernel.graph import RuntimeFactory, RuntimeGraph
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
        RuntimeComponentName.WORKSPACE_STATE,
        RuntimeComponentName.POLICY,
        RuntimeComponentName.SANDBOX,
        RuntimeComponentName.COMMAND,
        RuntimeComponentName.MUTATION,
        RuntimeComponentName.TOOLS,
        RuntimeComponentName.VERIFICATION,
        RuntimeComponentName.INSTRUCTIONS,
        RuntimeComponentName.MODEL,
        RuntimeComponentName.CONTEXT,
        RuntimeComponentName.PLANNER,
    ]
    assert graph.state(RuntimeComponentName.PLANNER) == RuntimeComponentState.READY
    assert graph.planner is not None
    assert graph.model_runtime is not None
    assert graph.tool_runtime is not None
