import sys
from pathlib import Path

import pytest

from singularity.sandbox import (
    LocalStagingBackend,
    SandboxCapabilityError,
    SandboxFilesystemMode,
    SandboxNetworkMode,
    SandboxNetworkPolicy,
    SandboxProfileName,
    SandboxRequest,
    SandboxResourceLimits,
    SandboxStatus,
    default_sandbox_profile,
)


def request_for(
    tmp_path: Path,
    command: list[str],
    *,
    timeout_seconds: int | None = 30,
    max_output_chars: int | None = None,
) -> SandboxRequest:
    profile = default_sandbox_profile(
        SandboxProfileName.ISOLATED_VERIFICATION,
        workspace_root=tmp_path,
    )
    profile.resources = SandboxResourceLimits(
        timeout_seconds=timeout_seconds,
        max_output_chars=max_output_chars,
        max_artifact_bytes=1024 * 1024,
    )
    return SandboxRequest(
        sandbox_id="sandbox_backend",
        session_id="session",
        task_id="task",
        action_id="action",
        command=command,
        cwd=tmp_path,
        workspace_root=tmp_path,
        profile=profile,
    )


def test_local_backend_capabilities_are_honest() -> None:
    capabilities = LocalStagingBackend().capabilities()

    # Local staging has no true filesystem isolation: it stages a workspace
    # copy on the same filesystem as the host. Report this honestly so the
    # manager does not over-promise isolation.
    assert capabilities.filesystem_isolation is False
    assert capabilities.copy_on_write is True
    assert capabilities.network_isolation is False
    assert capabilities.memory_limit is False
    assert capabilities.artifact_capture is True


def test_simple_command_runs_in_sandbox_and_write_does_not_touch_real_workspace(tmp_path: Path) -> None:
    backend = LocalStagingBackend()
    request = request_for(
        tmp_path,
        [
            sys.executable,
            "-c",
            "from pathlib import Path; Path('sandbox.txt').write_text('sandbox', encoding='utf-8'); print('done')",
        ],
    )

    prepared = backend.prepare(request)
    result = backend.run(prepared)
    backend.cleanup(prepared)

    assert result.status == SandboxStatus.SUCCESS
    assert result.exit_code == 0
    assert result.stdout.strip() == "done"
    assert not (tmp_path / "sandbox.txt").exists()
    assert result.filesystem_changes.created_files == ["sandbox.txt"]


def test_timeout_and_output_limit_are_enforced(tmp_path: Path) -> None:
    backend = LocalStagingBackend()
    timeout_request = request_for(
        tmp_path,
        [sys.executable, "-c", "import time; time.sleep(5)"],
        timeout_seconds=1,
    )
    output_request = request_for(
        tmp_path,
        [sys.executable, "-c", "print('A' * 200)"],
        max_output_chars=20,
    )

    timeout_prepared = backend.prepare(timeout_request)
    timeout_result = backend.run(timeout_prepared)
    backend.cleanup(timeout_prepared)
    output_prepared = backend.prepare(output_request)
    output_result = backend.run(output_prepared)
    backend.cleanup(output_prepared)

    assert timeout_result.status == SandboxStatus.TIMEOUT
    assert len(output_result.stdout) <= 20
    assert output_result.metadata["output_truncated"] is True


def test_hard_network_isolation_request_fails_closed(tmp_path: Path) -> None:
    backend = LocalStagingBackend()
    request = request_for(tmp_path, [sys.executable, "-c", "print('no network')"])
    request.profile.network = SandboxNetworkPolicy(
        mode=SandboxNetworkMode.DENIED,
        require_hard_isolation=True,
    )

    with pytest.raises(SandboxCapabilityError):
        backend.prepare(request)


def test_network_allowlist_metadata_does_not_require_hard_isolation(tmp_path: Path) -> None:
    backend = LocalStagingBackend()
    request = request_for(tmp_path, [sys.executable, "-c", "print('metadata only')"])
    request.profile.network = SandboxNetworkPolicy(
        mode=SandboxNetworkMode.ALLOWLIST,
        allowed_hosts=["pypi.org"],
    )

    prepared = backend.prepare(request)
    result = backend.run(prepared)
    backend.cleanup(prepared)

    assert result.status == SandboxStatus.SUCCESS
    assert result.metadata["network_isolation_enforced"] is False


def test_artifact_collection_captures_declared_paths(tmp_path: Path) -> None:
    backend = LocalStagingBackend()
    request = request_for(
        tmp_path,
        [
            sys.executable,
            "-c",
            "from pathlib import Path; Path('report.txt').write_text('report', encoding='utf-8')",
        ],
    )
    request.profile.filesystem.artifact_paths = ["report.txt"]

    prepared = backend.prepare(request)
    result = backend.run(prepared)
    backend.cleanup(prepared)

    assert any(artifact.relative_path == "report.txt" for artifact in result.artifacts)


def test_read_only_workspace_blocks_writes_to_staged_files(tmp_path: Path) -> None:
    (tmp_path / "data.txt").write_text("original", encoding="utf-8")
    backend = LocalStagingBackend()
    request = request_for(tmp_path, [sys.executable, "-c", "print('readonly')"])
    request.profile.filesystem.mode = SandboxFilesystemMode.READ_ONLY_WORKSPACE
    request.profile.filesystem.detect_changes = False

    prepared = backend.prepare(request)

    staged_file = prepared.workspace_copy_root / "data.txt"
    assert staged_file.exists()
    # READ_ONLY_WORKSPACE must actually enforce read-only: writing to a
    # staged file must fail rather than silently succeeding. This regression
    # test guards against the old bug where read_only was mapped to
    # COPY_ON_WRITE_WORKSPACE (writable) instead of READ_ONLY_WORKSPACE.
    with pytest.raises(PermissionError):
        staged_file.write_text("modified", encoding="utf-8")
    # Cleanup must succeed even when the tree was marked read-only.
    backend.cleanup(prepared)
    assert not prepared.sandbox_root.exists()


def test_read_only_workspace_mode_not_mapped_to_copy_on_write(tmp_path: Path) -> None:
    (tmp_path / "seed.txt").write_text("seed", encoding="utf-8")
    backend = LocalStagingBackend()
    request = request_for(tmp_path, [sys.executable, "-c", "print('mode')"])
    request.profile.filesystem.mode = SandboxFilesystemMode.READ_ONLY_WORKSPACE
    request.profile.filesystem.detect_changes = False

    prepared = backend.prepare(request)
    try:
        # The staged workspace must be the read-only mode, not silently
        # downgraded to copy-on-write (which would permit writes).
        import stat as stat_module
        staged = prepared.workspace_copy_root / "seed.txt"
        mode = staged.stat().st_mode
        assert not (mode & stat_module.S_IWUSR), "staged file retains write bit"
    finally:
        backend.cleanup(prepared)
