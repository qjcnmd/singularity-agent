from __future__ import annotations

import sys
from pathlib import Path

import pytest

from singularity.sandbox.backends import DockerSandboxBackend, docker_backend_available
from singularity.sandbox.models import (
    SandboxNetworkMode,
    SandboxNetworkPolicy,
    SandboxProfileName,
    SandboxRequest,
    SandboxStatus,
    default_sandbox_profile,
)


def _request(tmp_path: Path) -> SandboxRequest:
    return SandboxRequest(
        sandbox_id="sandbox_docker",
        session_id="session",
        task_id="task",
        action_id="action",
        command=[sys.executable, "-c", "print('docker')"],
        cwd=tmp_path,
        workspace_root=tmp_path,
        profile=default_sandbox_profile(
            SandboxProfileName.ISOLATED_VERIFICATION,
            workspace_root=tmp_path,
        ),
    )


def test_docker_backend_reports_hard_isolation_capabilities() -> None:
    backend = DockerSandboxBackend(image="python:3.13-slim")

    capabilities = backend.capabilities()

    assert backend.name() == "docker"
    assert capabilities.filesystem_isolation is True
    assert capabilities.network_isolation is True
    assert capabilities.memory_limit is True
    assert capabilities.process_limit is True


def test_docker_backend_builds_cli_command_with_staged_workspace_and_limits(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    completed_commands: list[list[str]] = []

    class _Completed:
        returncode = 0
        stdout = b"ok\n"
        stderr = b""

    def fake_run(command: list[str], **kwargs: object) -> _Completed:
        completed_commands.append(command)
        return _Completed()

    monkeypatch.setattr("singularity.sandbox.backends.subprocess.run", fake_run)
    request = _request(tmp_path)
    request.profile.network = SandboxNetworkPolicy(
        mode=SandboxNetworkMode.DENIED,
        require_hard_isolation=True,
    )
    request.profile.env.extra_env["SINGULARITY_SAFE_VAR"] = "safe-value"
    request.profile.resources.max_memory_mb = 128
    request.profile.resources.max_processes = 32
    backend = DockerSandboxBackend(image="python:3.13-slim")
    prepared = backend.prepare(request)

    result = backend.run(prepared)

    assert result.status == SandboxStatus.SUCCESS
    assert result.stdout == "ok\n"
    command = completed_commands[0]
    assert command[:3] == ["docker", "run", "--rm"]
    assert "--network" in command
    assert "none" in command
    assert "--memory" in command
    assert "128m" in command
    assert "--pids-limit" in command
    assert "32" in command
    assert "SINGULARITY_SAFE_VAR=safe-value" not in command
    assert "--env-file" in command
    env_file = Path(command[command.index("--env-file") + 1])
    assert env_file.is_file()
    assert "SINGULARITY_SAFE_VAR=safe-value" in env_file.read_text(encoding="utf-8")
    assert f"{prepared.workspace_copy_root}:/workspace" in command


def test_docker_backend_marks_missing_cli_as_backend_unavailable(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    def fake_run(command: list[str], **kwargs: object) -> object:
        raise FileNotFoundError("docker")

    monkeypatch.setattr("singularity.sandbox.backends.subprocess.run", fake_run)
    backend = DockerSandboxBackend(image="python:3.13-slim")
    prepared = backend.prepare(_request(tmp_path))

    result = backend.run(prepared)

    assert result.status == SandboxStatus.BACKEND_UNAVAILABLE
    assert result.metadata["error_code"] == "backend_unavailable"


def test_docker_backend_redacts_internal_paths_from_docker_errors(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    class _Completed:
        returncode = 125
        stdout = b""
        stderr = b""

    def fake_run(command: list[str], **kwargs: object) -> _Completed:
        completed = _Completed()
        completed.stderr = f"docker failed mounting {command[command.index('-v') + 1]}".encode()
        return completed

    monkeypatch.setattr("singularity.sandbox.backends.subprocess.run", fake_run)
    backend = DockerSandboxBackend(image="python:3.13-slim")
    prepared = backend.prepare(_request(tmp_path))

    result = backend.run(prepared)

    assert result.status == SandboxStatus.FAILED
    assert str(prepared.workspace_copy_root) not in result.stderr
    assert prepared.workspace_copy_root.as_posix() not in result.stderr
    assert "<sandbox-workspace>" in result.stderr


def test_docker_backend_availability_is_cached(monkeypatch: pytest.MonkeyPatch) -> None:
    calls = 0

    class _Completed:
        returncode = 1

    def fake_run(command: list[str], **kwargs: object) -> _Completed:
        nonlocal calls
        calls += 1
        return _Completed()

    monkeypatch.setattr("singularity.sandbox.backends._DOCKER_AVAILABILITY_CACHE", None)
    monkeypatch.setattr("singularity.sandbox.backends.shutil.which", lambda name: "docker")
    monkeypatch.setattr("singularity.sandbox.backends.subprocess.run", fake_run)

    assert docker_backend_available() is False
    assert docker_backend_available() is False
    assert calls == 1


def test_real_docker_backend_smoke_skips_when_daemon_unavailable(tmp_path: Path) -> None:
    if not docker_backend_available(use_cache=False):
        pytest.skip("Docker CLI or daemon is unavailable.")
    backend = DockerSandboxBackend()
    prepared = backend.prepare(_request(tmp_path))
    try:
        result = backend.run(prepared)
    finally:
        backend.cleanup(prepared)

    assert result.status == SandboxStatus.SUCCESS
    assert "docker" in result.stdout
