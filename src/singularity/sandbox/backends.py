from __future__ import annotations

import os
import signal
import shutil
import subprocess
import time
from datetime import UTC, datetime
from pathlib import Path
from typing import Protocol

from singularity.observability.redaction import TraceRedactor
from singularity.sandbox.artifacts import SandboxArtifactCollector
from singularity.sandbox.environment import SandboxEnvironmentBuilder
from singularity.sandbox.exceptions import SandboxCapabilityError
from singularity.sandbox.filesystem import SandboxFilesystemManager, random_trace_id
from singularity.sandbox.models import (
    PreparedSandbox,
    SandboxCapabilities,
    SandboxChangeSummary,
    SandboxNetworkMode,
    SandboxRequest,
    SandboxResult,
    SandboxStatus,
)


class SandboxBackend(Protocol):
    def name(self) -> str:
        ...

    def capabilities(self) -> SandboxCapabilities:
        ...

    def prepare(self, request: SandboxRequest) -> PreparedSandbox:
        ...

    def run(self, prepared: PreparedSandbox) -> SandboxResult:
        ...

    def cleanup(self, prepared: PreparedSandbox) -> None:
        ...


class LocalStagingBackend:
    def __init__(self) -> None:
        self.filesystem = SandboxFilesystemManager()
        self.environment = SandboxEnvironmentBuilder()
        self.artifacts = SandboxArtifactCollector()
        self.redactor = TraceRedactor()

    def name(self) -> str:
        return "local_staging"

    def capabilities(self) -> SandboxCapabilities:
        return SandboxCapabilities(
            filesystem_isolation=True,
            copy_on_write=True,
            readonly_mount=False,
            network_isolation=False,
            env_isolation=True,
            process_tree_kill=True,
            timeout=True,
            output_limit=True,
            memory_limit=False,
            process_limit=False,
            artifact_capture=True,
            change_detection=True,
        )

    def prepare(self, request: SandboxRequest) -> PreparedSandbox:
        self._ensure_capabilities(request)
        fs = self.filesystem.prepare_filesystem(
            sandbox_id=request.sandbox_id,
            policy=request.profile.filesystem,
            cwd=request.cwd,
        )
        env = self.environment.build_env(request.profile.env, os.environ)
        prepared = PreparedSandbox(
            sandbox_id=request.sandbox_id,
            backend_name=self.name(),
            sandbox_root=fs.sandbox_root,
            workspace_copy_root=fs.workspace_copy_root,
            execution_cwd=fs.execution_cwd,
            env=env,
            request=request,
            created_at=_now(),
            trace_id=random_trace_id(),
        )
        prepared.baseline = (
            self.filesystem.capture_baseline(prepared.workspace_copy_root)
            if request.profile.filesystem.detect_changes
            else {}
        )
        return prepared

    def run(self, prepared: PreparedSandbox) -> SandboxResult:
        started_at = _now()
        started = time.perf_counter()
        request = prepared.request
        process: subprocess.Popen[bytes] | None = None
        stdout = ""
        stderr = ""
        status = SandboxStatus.FAILED
        exit_code: int | None = None
        timed_out = False
        output_truncated = False
        error_code: str | None = None
        try:
            process = _start_process(
                request.command,
                cwd=prepared.execution_cwd,
                env=prepared.env,
            )
            try:
                raw_stdout, raw_stderr = process.communicate(
                    timeout=request.profile.resources.timeout_seconds
                )
            except subprocess.TimeoutExpired:
                timed_out = True
                error_code = "timeout"
                raw_stdout, raw_stderr = _communicate_after_kill(process)
            stdout = self.redactor.redact_text(raw_stdout.decode("utf-8", errors="replace"))
            stderr = self.redactor.redact_text(raw_stderr.decode("utf-8", errors="replace"))
            max_output = request.profile.resources.max_output_chars
            if max_output is not None:
                combined = len(stdout) + len(stderr)
                if combined > max_output:
                    output_truncated = True
                    stdout_budget = min(len(stdout), max_output)
                    stderr_budget = max(0, max_output - stdout_budget)
                    stdout = stdout[:stdout_budget]
                    stderr = stderr[:stderr_budget]
            exit_code = process.returncode
            status = SandboxStatus.TIMEOUT if timed_out else SandboxStatus.SUCCESS if exit_code == 0 else SandboxStatus.FAILED
        except FileNotFoundError as exc:
            error_code = "command_not_found"
            stderr = self.redactor.redact_text(str(exc))
            status = SandboxStatus.FAILED
        except PermissionError as exc:
            error_code = "permission_error"
            stderr = self.redactor.redact_text(str(exc))
            status = SandboxStatus.FAILED
        except Exception as exc:
            error_code = "sandbox_execution_error"
            stderr = self.redactor.redact_text(str(exc))
            status = SandboxStatus.FAILED
        changes = (
            self.filesystem.detect_changes(prepared.workspace_copy_root, prepared.baseline)
            if request.profile.filesystem.detect_changes
            else SandboxChangeSummary()
        )
        artifacts = self.artifacts.collect(
            sandbox_id=prepared.sandbox_id,
            workspace_root=prepared.workspace_copy_root,
            artifact_root=prepared.sandbox_root / "artifacts",
            artifact_paths=request.profile.filesystem.artifact_paths,
            limits=request.profile.resources,
            stdout=stdout,
            stderr=stderr,
        )
        metadata = {
            "output_truncated": output_truncated,
            "error_code": error_code,
            "network_isolation_enforced": False,
        }
        return SandboxResult(
            sandbox_id=prepared.sandbox_id,
            backend_name=self.name(),
            status=status,
            exit_code=exit_code,
            stdout=stdout,
            stderr=stderr,
            started_at=started_at,
            ended_at=_now(),
            duration_ms=int((time.perf_counter() - started) * 1000),
            artifacts=artifacts,
            filesystem_changes=changes,
            trace_id=prepared.trace_id,
            cleanup_status="pending",
            metadata=metadata,
        )

    def cleanup(self, prepared: PreparedSandbox) -> None:
        self.filesystem.cleanup(prepared.sandbox_root)

    def _ensure_capabilities(self, request: SandboxRequest) -> None:
        capabilities = self.capabilities()
        if request.profile.network.require_hard_isolation and not capabilities.network_isolation:
            raise SandboxCapabilityError("Backend cannot enforce required network isolation.")
        if request.profile.resources.max_memory_mb is not None and not capabilities.memory_limit:
            raise SandboxCapabilityError("Backend cannot enforce memory limits.")
        if request.profile.resources.max_processes is not None and not capabilities.process_limit:
            raise SandboxCapabilityError("Backend cannot enforce process limits.")


class DockerSandboxBackend:
    def __init__(self, *, image: str = "python:3.13-slim") -> None:
        self.image = image
        self.filesystem = SandboxFilesystemManager()
        self.environment = SandboxEnvironmentBuilder()
        self.artifacts = SandboxArtifactCollector()
        self.redactor = TraceRedactor()

    def name(self) -> str:
        return "docker"

    def capabilities(self) -> SandboxCapabilities:
        return SandboxCapabilities(
            filesystem_isolation=True,
            copy_on_write=True,
            readonly_mount=False,
            network_isolation=True,
            env_isolation=True,
            process_tree_kill=True,
            timeout=True,
            output_limit=True,
            memory_limit=True,
            process_limit=True,
            artifact_capture=True,
            change_detection=True,
        )

    def is_available(self) -> bool:
        return docker_backend_available()

    def prepare(self, request: SandboxRequest) -> PreparedSandbox:
        self._ensure_capabilities(request)
        self.ensure_request_supported(request)
        fs = self.filesystem.prepare_filesystem(
            sandbox_id=request.sandbox_id,
            policy=request.profile.filesystem,
            cwd=request.cwd,
        )
        env = self.environment.build_env(request.profile.env, os.environ)
        prepared = PreparedSandbox(
            sandbox_id=request.sandbox_id,
            backend_name=self.name(),
            sandbox_root=fs.sandbox_root,
            workspace_copy_root=fs.workspace_copy_root,
            execution_cwd=fs.execution_cwd,
            env=env,
            request=request,
            created_at=_now(),
            trace_id=random_trace_id(),
        )
        prepared.baseline = (
            self.filesystem.capture_baseline(prepared.workspace_copy_root)
            if request.profile.filesystem.detect_changes
            else {}
        )
        return prepared

    def run(self, prepared: PreparedSandbox) -> SandboxResult:
        started_at = _now()
        started = time.perf_counter()
        request = prepared.request
        stdout = ""
        stderr = ""
        exit_code: int | None = None
        output_truncated = False
        error_code: str | None = None
        status = SandboxStatus.FAILED
        try:
            completed = subprocess.run(
                self._docker_command(prepared),
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
                timeout=request.profile.resources.timeout_seconds,
            )
            exit_code = completed.returncode
            stdout = self._redact_output(
                completed.stdout.decode("utf-8", errors="replace"),
                prepared,
            )
            stderr = self._redact_output(
                completed.stderr.decode("utf-8", errors="replace"),
                prepared,
            )
            if exit_code != 0 and _looks_like_docker_unavailable(stderr):
                status = SandboxStatus.BACKEND_UNAVAILABLE
                error_code = "backend_unavailable"
            else:
                status = SandboxStatus.SUCCESS if exit_code == 0 else SandboxStatus.FAILED
        except subprocess.TimeoutExpired as exc:
            error_code = "timeout"
            status = SandboxStatus.TIMEOUT
            stdout = self._redact_output(
                (exc.stdout or b"").decode("utf-8", errors="replace")
                if isinstance(exc.stdout, bytes)
                else str(exc.stdout or ""),
                prepared,
            )
            stderr = self._redact_output(
                (exc.stderr or b"").decode("utf-8", errors="replace")
                if isinstance(exc.stderr, bytes)
                else str(exc.stderr or ""),
                prepared,
            )
        except FileNotFoundError as exc:
            error_code = "backend_unavailable"
            status = SandboxStatus.BACKEND_UNAVAILABLE
            stderr = self._redact_output(str(exc), prepared)
        except Exception as exc:
            error_code = "sandbox_execution_error"
            status = SandboxStatus.FAILED
            stderr = self._redact_output(str(exc), prepared)

        max_output = request.profile.resources.max_output_chars
        if max_output is not None:
            combined = len(stdout) + len(stderr)
            if combined > max_output:
                output_truncated = True
                stdout_budget = min(len(stdout), max_output)
                stderr_budget = max(0, max_output - stdout_budget)
                stdout = stdout[:stdout_budget]
                stderr = stderr[:stderr_budget]

        changes = (
            self.filesystem.detect_changes(prepared.workspace_copy_root, prepared.baseline)
            if request.profile.filesystem.detect_changes
            else SandboxChangeSummary()
        )
        artifacts = self.artifacts.collect(
            sandbox_id=prepared.sandbox_id,
            workspace_root=prepared.workspace_copy_root,
            artifact_root=prepared.sandbox_root / "artifacts",
            artifact_paths=request.profile.filesystem.artifact_paths,
            limits=request.profile.resources,
            stdout=stdout,
            stderr=stderr,
        )
        metadata = {
            "output_truncated": output_truncated,
            "error_code": error_code,
            "network_isolation_enforced": request.profile.network.mode
            != SandboxNetworkMode.ALLOWED,
            "image": self.image,
        }
        return SandboxResult(
            sandbox_id=prepared.sandbox_id,
            backend_name=self.name(),
            status=status,
            exit_code=exit_code,
            stdout=stdout,
            stderr=stderr,
            started_at=started_at,
            ended_at=_now(),
            duration_ms=int((time.perf_counter() - started) * 1000),
            artifacts=artifacts,
            filesystem_changes=changes,
            trace_id=prepared.trace_id,
            cleanup_status="pending",
            metadata=metadata,
        )

    def cleanup(self, prepared: PreparedSandbox) -> None:
        self.filesystem.cleanup(prepared.sandbox_root)

    def _redact_output(self, text: str, prepared: PreparedSandbox) -> str:
        redacted = self.redactor.redact_text(text)
        replacements = {
            str(prepared.workspace_copy_root): "<sandbox-workspace>",
            prepared.workspace_copy_root.as_posix(): "<sandbox-workspace>",
            str(prepared.sandbox_root): "<sandbox-root>",
            prepared.sandbox_root.as_posix(): "<sandbox-root>",
        }
        for raw, handle in sorted(replacements.items(), key=lambda item: len(item[0]), reverse=True):
            if raw:
                redacted = redacted.replace(raw, handle)
        return redacted

    def _docker_command(self, prepared: PreparedSandbox) -> list[str]:
        request = prepared.request
        workdir = self._container_workdir(prepared)
        command = [
            "docker",
            "run",
            "--rm",
            "-v",
            f"{prepared.workspace_copy_root}:/workspace",
            "-w",
            workdir,
        ]
        if request.profile.network.mode != SandboxNetworkMode.ALLOWED:
            command.extend(["--network", "none"])
        if request.profile.resources.max_memory_mb is not None:
            command.extend(["--memory", f"{request.profile.resources.max_memory_mb}m"])
        if request.profile.resources.max_processes is not None:
            command.extend(["--pids-limit", str(request.profile.resources.max_processes)])
        env_file = self._write_env_file(prepared)
        if env_file is not None:
            command.extend(["--env-file", str(env_file)])
        command.append(self.image)
        command.extend(_container_command(request.command))
        return command

    @staticmethod
    def _write_env_file(prepared: PreparedSandbox) -> Path | None:
        if not prepared.env:
            return None
        env_file = prepared.sandbox_root / "docker.env"
        lines = [
            f"{key}={_escape_env_file_value(value)}"
            for key, value in sorted(prepared.env.items())
        ]
        env_file.write_text("\n".join(lines) + "\n", encoding="utf-8")
        return env_file

    @staticmethod
    def _container_workdir(prepared: PreparedSandbox) -> str:
        try:
            relative = prepared.execution_cwd.relative_to(prepared.workspace_copy_root)
        except ValueError:
            return "/workspace"
        suffix = relative.as_posix()
        return "/workspace" if suffix == "." else f"/workspace/{suffix}"

    def _ensure_capabilities(self, request: SandboxRequest) -> None:
        if (
            request.profile.network.mode == SandboxNetworkMode.ALLOWLIST
            and request.profile.network.allowed_hosts
        ):
            raise SandboxCapabilityError("Docker backend cannot enforce host allowlists.")

    def ensure_request_supported(self, request: SandboxRequest) -> None:
        command = _container_command(request.command)
        if not command:
            return
        program = Path(command[0]).name.lower()
        if program in {"python", "python3", "sh", "bash"}:
            return
        raise SandboxCapabilityError(
            "Docker backend default image does not provide the requested project toolchain."
        )


_DOCKER_AVAILABILITY_CACHE: tuple[float, bool] | None = None
_DOCKER_AVAILABILITY_TTL_SECONDS = 30.0


def docker_backend_available(*, use_cache: bool = True) -> bool:
    global _DOCKER_AVAILABILITY_CACHE
    now = time.monotonic()
    if (
        use_cache
        and _DOCKER_AVAILABILITY_CACHE is not None
        and now - _DOCKER_AVAILABILITY_CACHE[0] < _DOCKER_AVAILABILITY_TTL_SECONDS
    ):
        return _DOCKER_AVAILABILITY_CACHE[1]
    if shutil.which("docker") is None:
        _DOCKER_AVAILABILITY_CACHE = (now, False)
        return False
    try:
        completed = subprocess.run(
            ["docker", "info"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
            timeout=5,
        )
    except (FileNotFoundError, subprocess.TimeoutExpired, OSError, TypeError):
        _DOCKER_AVAILABILITY_CACHE = (now, False)
        return False
    available = completed.returncode == 0
    _DOCKER_AVAILABILITY_CACHE = (now, available)
    return available


def default_sandbox_backends() -> list[SandboxBackend]:
    if docker_backend_available():
        return [DockerSandboxBackend(), LocalStagingBackend()]
    return [LocalStagingBackend()]


def _now() -> str:
    return datetime.now(UTC).isoformat()


def _start_process(
    command: list[str] | str,
    *,
    cwd: Path,
    env: dict[str, str],
) -> subprocess.Popen[bytes]:
    shell = isinstance(command, str)
    kwargs: dict[str, object] = {
        "cwd": str(cwd),
        "env": env,
        "stdin": subprocess.DEVNULL,
        "stdout": subprocess.PIPE,
        "stderr": subprocess.PIPE,
        "shell": shell,
    }
    if os.name == "nt":
        kwargs["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
    else:
        kwargs["start_new_session"] = True
    return subprocess.Popen(command, **kwargs)


def _kill_process_tree(process: subprocess.Popen[bytes]) -> None:
    if process.poll() is not None:
        return
    if os.name == "nt":
        try:
            subprocess.run(
                ["taskkill", "/PID", str(process.pid), "/T", "/F"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
                timeout=5,
            )
        except Exception:
            process.kill()
        return
    try:
        os.killpg(os.getpgid(process.pid), signal.SIGTERM)
    except Exception:
        process.terminate()
    try:
        process.wait(timeout=1)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(os.getpgid(process.pid), signal.SIGKILL)
        except Exception:
            process.kill()


def _communicate_after_kill(process: subprocess.Popen[bytes]) -> tuple[bytes, bytes]:
    _kill_process_tree(process)
    try:
        return process.communicate(timeout=2)
    except subprocess.TimeoutExpired:
        try:
            if process.poll() is None:
                process.kill()
        except Exception:
            pass
        try:
            return process.communicate(timeout=1)
        except subprocess.TimeoutExpired:
            return b"", b""


def _container_command(command: list[str] | str) -> list[str]:
    if isinstance(command, str):
        return ["sh", "-lc", command]
    if not command:
        return []
    executable = Path(str(command[0])).name.lower()
    if executable.startswith("python"):
        return ["python", *[str(part) for part in command[1:]]]
    return [str(part) for part in command]


def _looks_like_docker_unavailable(stderr: str) -> bool:
    lowered = stderr.lower()
    markers = (
        "cannot connect to the docker daemon",
        "docker daemon is not running",
        "error during connect",
        "is the docker daemon running",
    )
    return any(marker in lowered for marker in markers)


def _escape_env_file_value(value: str) -> str:
    return str(value).replace("\r", "\\r").replace("\n", "\\n")
