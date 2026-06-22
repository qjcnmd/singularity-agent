from __future__ import annotations

from pathlib import Path

from singularity.config import ProductionRuntimeConfig
from singularity.kernel.bootstrap import KernelBootstrap
from singularity.kernel.exceptions import KernelBootstrapError
from singularity.kernel.graph import RuntimeFactory
from singularity.kernel.models import KernelStatus


def test_kernel_bootstrap_creates_ready_kernel_and_releases_lock_on_shutdown(
    tmp_path: Path,
    monkeypatch,
) -> None:
    monkeypatch.setenv("SINGULARITY_API_KEY", "test")
    monkeypatch.setenv("SINGULARITY_BASE_URL", "http://localhost/v1")
    monkeypatch.setenv("SINGULARITY_MODEL", "test-model")
    config = ProductionRuntimeConfig.from_cli(project_root=tmp_path, dry_run=True)

    kernel = KernelBootstrap(project_root=tmp_path, config=config).boot("Build kernel")

    assert kernel.context.status == KernelStatus.READY
    assert kernel.context.workspace_lock_status == "acquired"
    assert (tmp_path / ".singularity" / "locks" / "workspace.lock").exists()

    kernel.shutdown()

    assert not (tmp_path / ".singularity" / "locks" / "workspace.lock").exists()


def test_kernel_bootstrap_records_effective_config_source_trace(
    tmp_path: Path,
    monkeypatch,
) -> None:
    monkeypatch.setenv("SINGULARITY_API_KEY", "test")
    monkeypatch.setenv("SINGULARITY_BASE_URL", "http://localhost/v1")
    monkeypatch.setenv("SINGULARITY_MODEL", "test-model")
    config_dir = tmp_path / ".singularity"
    config_dir.mkdir()
    (config_dir / "config.toml").write_text("max_turns = 5\n", encoding="utf-8")
    config = ProductionRuntimeConfig.from_cli(project_root=tmp_path)

    kernel = KernelBootstrap(project_root=tmp_path, config=config).boot("Build kernel")

    config_events = [
        event
        for event in kernel.graph.trace.store.query_events()
        if event.runtime == "config" and event.summary == "Effective runtime config resolved."
    ]
    assert config_events
    assert config_events[-1].payload["values"]["max_turns"] == 5
    assert (
        config_events[-1].payload["sources"]["max_turns"]
        == "config:.singularity/config.toml"
    )

    kernel.shutdown()


def test_kernel_bootstrap_failure_releases_lock_and_returns_partial_final_report(
    tmp_path: Path,
    monkeypatch,
) -> None:
    monkeypatch.setenv("SINGULARITY_API_KEY", "test")
    monkeypatch.setenv("SINGULARITY_BASE_URL", "http://localhost/v1")
    monkeypatch.setenv("SINGULARITY_MODEL", "test-model")
    config = ProductionRuntimeConfig.from_cli(project_root=tmp_path, dry_run=True)

    class FailingFactory(RuntimeFactory):
        def build(self, **kwargs):
            raise RuntimeError("graph failed")

    try:
        KernelBootstrap(
            project_root=tmp_path,
            config=config,
            runtime_factory=FailingFactory(),
        ).boot("Build kernel")
    except KernelBootstrapError as exc:
        report = exc.final_report
    else:
        raise AssertionError("KernelBootstrapError was not raised.")

    assert not (tmp_path / ".singularity" / "locks" / "workspace.lock").exists()
    assert report is not None
    assert report.shutdown_reason == "bootstrap_failed"
    assert report.cleanup_status == "completed"
    assert report.diagnostics_count == 1
    assert report.workspace_lock_status == "released"
