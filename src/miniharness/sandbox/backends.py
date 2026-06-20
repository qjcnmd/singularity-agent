from __future__ import annotations

import os
import signal
import subprocess
import time
from datetime import UTC, datetime
from pathlib import Path
from typing import Protocol

from miniharness.observability.redaction import TraceRedactor
from miniharness.sandbox.artifacts import SandboxArtifactCollector
from miniharness.sandbox.environment import SandboxEnvironmentBuilder
from miniharness.sandbox.exceptions import SandboxCapabilityError
from miniharness.sandbox.filesystem import SandboxFilesystemManager, random_trace_id
from miniharness.sandbox.models import (
    PreparedSandbox,
    SandboxCapabilities,
    SandboxChangeSummary,
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
