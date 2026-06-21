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
