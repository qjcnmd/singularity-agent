from __future__ import annotations

import subprocess
import sys
from pathlib import Path

import pytest

from singularity.sandbox.backends import DockerSandboxBackend, docker_backend_available
from singularity.sandbox.models import (
    SandboxFilesystemMode,
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
    # Security hardening params (P1-3): --name, --user, --cap-drop=ALL,
    # --security-opt no-new-privileges, --init must be present.
    assert "--name" in command
    assert command[command.index("--name") + 1] == f"singularity-{prepared.sandbox_id}"
    assert "--user" in command
    assert command[command.index("--user") + 1] == "1000:1000"
    assert "--cap-drop=ALL" in command
    assert "--security-opt" in command
    assert command[command.index("--security-opt") + 1] == "no-new-privileges"
    assert "--init" in command
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


def test_docker_command_pins_image_digest_when_provided(
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
    request.profile.image_digest = "sha256:abcdef1234567890"
    backend = DockerSandboxBackend(image="python:3.13-slim")
    prepared = backend.prepare(request)

    backend.run(prepared)

    command = completed_commands[0]
    assert "python:3.13-slim@sha256:abcdef1234567890" in command
    # The bare tag must not appear separately when a digest is pinned.
    assert "python:3.13-slim" not in [
        part for part in command if part == "python:3.13-slim"
    ]


def test_docker_command_uses_image_tag_when_no_digest(
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
    backend = DockerSandboxBackend(image="python:3.13-slim")
    prepared = backend.prepare(request)

    backend.run(prepared)

    command = completed_commands[0]
    assert "python:3.13-slim" in command
    # No digest suffix must be appended when image_digest is None.
    assert not any(
        part.startswith("python:3.13-slim@") for part in command
    )


def test_docker_command_mounts_workspace_readonly_for_read_only_mode(
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
    request.profile.filesystem.mode = SandboxFilesystemMode.READ_ONLY_WORKSPACE
    backend = DockerSandboxBackend(image="python:3.13-slim")
    prepared = backend.prepare(request)

    backend.run(prepared)

    command = completed_commands[0]
    readonly_mount = f"{prepared.workspace_copy_root}:/workspace:ro"
    assert readonly_mount in command


def test_docker_command_uses_memory_and_pids_limit_fields(
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
    request.profile.resources.memory_limit = "512m"
    request.profile.resources.pids_limit = 100
    backend = DockerSandboxBackend(image="python:3.13-slim")
    prepared = backend.prepare(request)

    backend.run(prepared)

    command = completed_commands[0]
    assert "--memory" in command
    assert command[command.index("--memory") + 1] == "512m"
    assert "--pids-limit" in command
    assert command[command.index("--pids-limit") + 1] == "100"


def test_docker_timeout_triggers_container_stop(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    run_calls: list[list[str]] = []

    class _Completed:
        returncode = 0
        stdout = b""
        stderr = b""

    def fake_run(command: list[str], **kwargs: object) -> _Completed:
        run_calls.append(command)
        if len(command) >= 3 and command[:3] == ["docker", "run", "--rm"]:
            raise subprocess.TimeoutExpired(cmd=command, timeout=1)
        return _Completed()

    monkeypatch.setattr("singularity.sandbox.backends.subprocess.run", fake_run)
    request = _request(tmp_path)
    request.profile.resources.timeout_seconds = 1
    backend = DockerSandboxBackend(image="python:3.13-slim")
    prepared = backend.prepare(request)

    result = backend.run(prepared)

    assert result.status == SandboxStatus.TIMEOUT
    assert result.metadata["error_code"] == "timeout"
    # On timeout, the backend must call `docker stop <container>` to clean
    # up the orphaned container (it was started with --rm, so stop also
    # removes it).
    stop_calls = [
        cmd for cmd in run_calls if len(cmd) >= 2 and cmd[:2] == ["docker", "stop"]
    ]
    assert len(stop_calls) == 1
    assert stop_calls[0][2] == f"singularity-{prepared.sandbox_id}"


def test_docker_command_respects_container_user_override(
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
    request.metadata["container_user"] = "1001:1001"
    backend = DockerSandboxBackend(image="python:3.13-slim")
    prepared = backend.prepare(request)

    backend.run(prepared)

    command = completed_commands[0]
    assert "--user" in command
    assert command[command.index("--user") + 1] == "1001:1001"
