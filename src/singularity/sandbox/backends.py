from __future__ import annotations

import ctypes
import os
import signal
import shutil
import subprocess
import time
from ctypes import wintypes
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
            process = self._start_process(prepared)
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

    def _start_process(self, prepared: PreparedSandbox):
        return _start_process(
            prepared.request.command,
            cwd=prepared.execution_cwd,
            env=prepared.env,
        )

    def _ensure_capabilities(self, request: SandboxRequest) -> None:
        capabilities = self.capabilities()
        if request.profile.network.require_hard_isolation and not capabilities.network_isolation:
            raise SandboxCapabilityError("Backend cannot enforce required network isolation.")
        if request.profile.resources.max_memory_mb is not None and not capabilities.memory_limit:
            raise SandboxCapabilityError("Backend cannot enforce memory limits.")
        if request.profile.resources.max_processes is not None and not capabilities.process_limit:
            raise SandboxCapabilityError("Backend cannot enforce process limits.")


class WindowsRestrictedTokenBackend(LocalStagingBackend):
    def name(self) -> str:
        return "windows_restricted_token"

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

    def is_available(self) -> bool:
        return windows_restricted_token_available()

    def prepare(self, request: SandboxRequest) -> PreparedSandbox:
        prepared = super().prepare(request)
        _mark_low_integrity(prepared.sandbox_root)
        return prepared

    def run(self, prepared: PreparedSandbox) -> SandboxResult:
        result = super().run(prepared)
        result.metadata["restricted_token"] = True
        result.metadata["integrity_level"] = "low"
        return result

    def _start_process(self, prepared: PreparedSandbox):
        return _start_windows_restricted_process(prepared)


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


_WINDOWS_RESTRICTED_TOKEN_AVAILABILITY_CACHE: tuple[float, bool] | None = None


def windows_restricted_token_available(*, use_cache: bool = True) -> bool:
    global _WINDOWS_RESTRICTED_TOKEN_AVAILABILITY_CACHE
    if os.name != "nt":
        return False
    now = time.monotonic()
    if (
        use_cache
        and _WINDOWS_RESTRICTED_TOKEN_AVAILABILITY_CACHE is not None
        and now - _WINDOWS_RESTRICTED_TOKEN_AVAILABILITY_CACHE[0] < _DOCKER_AVAILABILITY_TTL_SECONDS
    ):
        return _WINDOWS_RESTRICTED_TOKEN_AVAILABILITY_CACHE[1]
    token = None
    try:
        token = _create_restricted_token()
        available = True
    except OSError:
        available = False
    finally:
        if token is not None:
            _close_handle(token)
    _WINDOWS_RESTRICTED_TOKEN_AVAILABILITY_CACHE = (now, available)
    return available


def default_sandbox_backends() -> list[SandboxBackend]:
    local = LocalStagingBackend()
    windows = WindowsRestrictedTokenBackend() if windows_restricted_token_available() else None
    if docker_backend_available():
        return [DockerSandboxBackend(), *([windows] if windows else []), local]
    return [*([windows] if windows else []), local]


if os.name == "nt":
    _HANDLE = wintypes.HANDLE
    _DWORD = wintypes.DWORD
    _BOOL = wintypes.BOOL
    _LPVOID = wintypes.LPVOID
    _LPCWSTR = wintypes.LPCWSTR
    _LPWSTR = wintypes.LPWSTR

    class _SECURITY_ATTRIBUTES(ctypes.Structure):
        _fields_ = [
            ("nLength", _DWORD),
            ("lpSecurityDescriptor", _LPVOID),
            ("bInheritHandle", _BOOL),
        ]

    class _STARTUPINFO(ctypes.Structure):
        _fields_ = [
            ("cb", _DWORD),
            ("lpReserved", _LPWSTR),
            ("lpDesktop", _LPWSTR),
            ("lpTitle", _LPWSTR),
            ("dwX", _DWORD),
            ("dwY", _DWORD),
            ("dwXSize", _DWORD),
            ("dwYSize", _DWORD),
            ("dwXCountChars", _DWORD),
            ("dwYCountChars", _DWORD),
            ("dwFillAttribute", _DWORD),
            ("dwFlags", _DWORD),
            ("wShowWindow", wintypes.WORD),
            ("cbReserved2", wintypes.WORD),
            ("lpReserved2", ctypes.POINTER(ctypes.c_byte)),
            ("hStdInput", _HANDLE),
            ("hStdOutput", _HANDLE),
            ("hStdError", _HANDLE),
        ]

    class _PROCESS_INFORMATION(ctypes.Structure):
        _fields_ = [
            ("hProcess", _HANDLE),
            ("hThread", _HANDLE),
            ("dwProcessId", _DWORD),
            ("dwThreadId", _DWORD),
        ]

    class _SID_AND_ATTRIBUTES(ctypes.Structure):
        _fields_ = [
            ("Sid", _LPVOID),
            ("Attributes", _DWORD),
        ]

    class _TOKEN_MANDATORY_LABEL(ctypes.Structure):
        _fields_ = [("Label", _SID_AND_ATTRIBUTES)]

    class _JOBOBJECT_BASIC_LIMIT_INFORMATION(ctypes.Structure):
        _fields_ = [
            ("PerProcessUserTimeLimit", ctypes.c_longlong),
            ("PerJobUserTimeLimit", ctypes.c_longlong),
            ("LimitFlags", _DWORD),
            ("MinimumWorkingSetSize", ctypes.c_size_t),
            ("MaximumWorkingSetSize", ctypes.c_size_t),
            ("ActiveProcessLimit", _DWORD),
            ("Affinity", ctypes.c_size_t),
            ("PriorityClass", _DWORD),
            ("SchedulingClass", _DWORD),
        ]

    class _IO_COUNTERS(ctypes.Structure):
        _fields_ = [
            ("ReadOperationCount", ctypes.c_ulonglong),
            ("WriteOperationCount", ctypes.c_ulonglong),
            ("OtherOperationCount", ctypes.c_ulonglong),
            ("ReadTransferCount", ctypes.c_ulonglong),
            ("WriteTransferCount", ctypes.c_ulonglong),
            ("OtherTransferCount", ctypes.c_ulonglong),
        ]

    class _JOBOBJECT_EXTENDED_LIMIT_INFORMATION(ctypes.Structure):
        _fields_ = [
            ("BasicLimitInformation", _JOBOBJECT_BASIC_LIMIT_INFORMATION),
            ("IoInfo", _IO_COUNTERS),
            ("ProcessMemoryLimit", ctypes.c_size_t),
            ("JobMemoryLimit", ctypes.c_size_t),
            ("PeakProcessMemoryUsed", ctypes.c_size_t),
            ("PeakJobMemoryUsed", ctypes.c_size_t),
        ]


_TOKEN_ASSIGN_PRIMARY = 0x0001
_TOKEN_DUPLICATE = 0x0002
_TOKEN_IMPERSONATE = 0x0004
_TOKEN_QUERY = 0x0008
_TOKEN_ADJUST_DEFAULT = 0x0080
_TOKEN_ADJUST_SESSIONID = 0x0100
_DISABLE_MAX_PRIVILEGE = 0x0001
_LUA_TOKEN = 0x0004
_SE_GROUP_INTEGRITY = 0x00000020
_TOKEN_INTEGRITY_LEVEL = 25
_CREATE_NO_WINDOW = 0x08000000
_CREATE_SUSPENDED = 0x00000004
_CREATE_UNICODE_ENVIRONMENT = 0x00000400
_STARTF_USESTDHANDLES = 0x00000100
_WAIT_OBJECT_0 = 0x00000000
_WAIT_TIMEOUT = 0x00000102
_INFINITE = 0xFFFFFFFF
_STILL_ACTIVE = 259
_JOB_OBJECT_EXTENDED_LIMIT_INFORMATION = 9
_JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE = 0x00002000


class _WindowsRestrictedProcess:
    def __init__(
        self,
        *,
        command: list[str] | str,
        process_handle: int,
        thread_handle: int,
        process_id: int,
        job_handle: int | None,
        job_assigned: bool,
        stdout_path: Path,
        stderr_path: Path,
        streams: list[object],
    ) -> None:
        self.args = command
        self.pid = process_id
        self.returncode: int | None = None
        self._process_handle = process_handle
        self._thread_handle = thread_handle
        self._job_handle = job_handle
        self._job_assigned = job_assigned
        self._stdout_path = stdout_path
        self._stderr_path = stderr_path
        self._streams = streams
        self._closed = False

    def communicate(self, timeout: int | float | None = None) -> tuple[bytes, bytes]:
        wait_ms = _INFINITE if timeout is None else max(1, int(float(timeout) * 1000))
        wait_result = _kernel32().WaitForSingleObject(self._process_handle, wait_ms)
        if wait_result == _WAIT_TIMEOUT:
            raise subprocess.TimeoutExpired(self.args, timeout)
        if wait_result != _WAIT_OBJECT_0:
            raise _last_winerror("WaitForSingleObject")
        self.returncode = self._exit_code()
        self._close_streams()
        stdout = self._stdout_path.read_bytes() if self._stdout_path.exists() else b""
        stderr = self._stderr_path.read_bytes() if self._stderr_path.exists() else b""
        self._close_handles()
        return stdout, stderr

    def poll(self) -> int | None:
        if self.returncode is not None:
            return self.returncode
        exit_code = wintypes.DWORD()
        if not _kernel32().GetExitCodeProcess(self._process_handle, ctypes.byref(exit_code)):
            return None
        if exit_code.value == _STILL_ACTIVE:
            return None
        self.returncode = int(exit_code.value)
        return self.returncode

    def kill(self) -> None:
        if self.poll() is not None:
            return
        if self._job_handle and self._job_assigned:
            _kernel32().TerminateJobObject(self._job_handle, 1)
        else:
            _kernel32().TerminateProcess(self._process_handle, 1)

    def _exit_code(self) -> int:
        exit_code = wintypes.DWORD()
        if not _kernel32().GetExitCodeProcess(self._process_handle, ctypes.byref(exit_code)):
            raise _last_winerror("GetExitCodeProcess")
        return int(exit_code.value)

    def _close_streams(self) -> None:
        for stream in self._streams:
            try:
                stream.close()
            except Exception:
                pass
        self._streams.clear()

    def _close_handles(self) -> None:
        if self._closed:
            return
        self._closed = True
        self._close_streams()
        _close_handle(self._thread_handle)
        _close_handle(self._process_handle)
        if self._job_handle:
            _close_handle(self._job_handle)


def _start_windows_restricted_process(prepared: PreparedSandbox) -> _WindowsRestrictedProcess:
    if os.name != "nt":
        raise OSError("Windows restricted token backend is only available on Windows.")
    import msvcrt

    token = _create_restricted_token()
    stdout_path = prepared.sandbox_root / "windows.stdout"
    stderr_path = prepared.sandbox_root / "windows.stderr"
    stdout_file = stdout_path.open("w+b")
    stderr_file = stderr_path.open("w+b")
    stdin_file = open(os.devnull, "rb")
    streams: list[object] = [stdout_file, stderr_file, stdin_file]
    job_handle: int | None = None
    try:
        for stream in streams:
            os.set_handle_inheritable(msvcrt.get_osfhandle(stream.fileno()), True)
        startup = _STARTUPINFO()
        startup.cb = ctypes.sizeof(_STARTUPINFO)
        startup.dwFlags = _STARTF_USESTDHANDLES
        startup.hStdInput = msvcrt.get_osfhandle(stdin_file.fileno())
        startup.hStdOutput = msvcrt.get_osfhandle(stdout_file.fileno())
        startup.hStdError = msvcrt.get_osfhandle(stderr_file.fileno())
        process_info = _PROCESS_INFORMATION()
        command_line = ctypes.create_unicode_buffer(_windows_command_line(prepared.request.command))
        env_block = ctypes.create_unicode_buffer(_windows_env_block(prepared.env))
        flags = _CREATE_UNICODE_ENVIRONMENT | _CREATE_NO_WINDOW | _CREATE_SUSPENDED
        ok = _advapi32().CreateProcessAsUserW(
            token,
            None,
            command_line,
            None,
            None,
            True,
            flags,
            env_block,
            str(prepared.execution_cwd),
            ctypes.byref(startup),
            ctypes.byref(process_info),
        )
        if not ok:
            raise _last_winerror("CreateProcessAsUserW")
        job_handle, job_assigned = _assign_kill_on_close_job(process_info.hProcess)
        _kernel32().ResumeThread(process_info.hThread)
        return _WindowsRestrictedProcess(
            command=prepared.request.command,
            process_handle=process_info.hProcess,
            thread_handle=process_info.hThread,
            process_id=process_info.dwProcessId,
            job_handle=job_handle,
            job_assigned=job_assigned,
            stdout_path=stdout_path,
            stderr_path=stderr_path,
            streams=streams,
        )
    except Exception:
        for stream in streams:
            try:
                stream.close()
            except Exception:
                pass
        if job_handle:
            _close_handle(job_handle)
        raise
    finally:
        _close_handle(token)


def _create_restricted_token() -> int:
    token = wintypes.HANDLE()
    restricted = wintypes.HANDLE()
    access = (
        _TOKEN_ASSIGN_PRIMARY
        | _TOKEN_DUPLICATE
        | _TOKEN_IMPERSONATE
        | _TOKEN_QUERY
        | _TOKEN_ADJUST_DEFAULT
        | _TOKEN_ADJUST_SESSIONID
    )
    if not _advapi32().OpenProcessToken(_kernel32().GetCurrentProcess(), access, ctypes.byref(token)):
        raise _last_winerror("OpenProcessToken")
    try:
        # ponytail: WRITE_RESTRICTED breaks CPython DLL init here; use staged FS for writes.
        flags = _DISABLE_MAX_PRIVILEGE | _LUA_TOKEN
        if not _advapi32().CreateRestrictedToken(
            token,
            flags,
            0,
            None,
            0,
            None,
            0,
            None,
            ctypes.byref(restricted),
        ):
            raise _last_winerror("CreateRestrictedToken")
        _set_low_integrity(restricted.value)
        return restricted.value
    finally:
        _close_handle(token.value)


def _set_low_integrity(token: int) -> None:
    sid = wintypes.LPVOID()
    if not _advapi32().ConvertStringSidToSidW("S-1-16-4096", ctypes.byref(sid)):
        raise _last_winerror("ConvertStringSidToSidW")
    try:
        label = _TOKEN_MANDATORY_LABEL()
        label.Label.Attributes = _SE_GROUP_INTEGRITY
        label.Label.Sid = sid
        size = ctypes.sizeof(label) + _advapi32().GetLengthSid(sid)
        if not _advapi32().SetTokenInformation(
            token,
            _TOKEN_INTEGRITY_LEVEL,
            ctypes.byref(label),
            size,
        ):
            raise _last_winerror("SetTokenInformation")
    finally:
        _kernel32().LocalFree(sid)


def _mark_low_integrity(path: Path) -> None:
    if os.name != "nt":
        raise OSError("Low integrity labels are only available on Windows.")
    completed = subprocess.run(
        ["icacls", str(path), "/setintegritylevel", "(OI)(CI)L", "/T"],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        check=False,
        timeout=10,
    )
    if completed.returncode != 0:
        stderr = completed.stderr.decode("utf-8", errors="replace").strip()
        raise OSError(f"icacls failed to set low integrity label: {stderr}")


def _assign_kill_on_close_job(process_handle: int) -> tuple[int | None, bool]:
    job = _kernel32().CreateJobObjectW(None, None)
    if not job:
        return None, False
    info = _JOBOBJECT_EXTENDED_LIMIT_INFORMATION()
    info.BasicLimitInformation.LimitFlags = _JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
    if not _kernel32().SetInformationJobObject(
        job,
        _JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
        ctypes.byref(info),
        ctypes.sizeof(info),
    ):
        _close_handle(job)
        return None, False
    assigned = bool(_kernel32().AssignProcessToJobObject(job, process_handle))
    return job, assigned


def _windows_command_line(command: list[str] | str) -> str:
    if isinstance(command, str):
        comspec = os.environ.get("COMSPEC") or "cmd.exe"
        return subprocess.list2cmdline([comspec, "/d", "/s", "/c", command])
    return subprocess.list2cmdline([str(part) for part in command])


def _windows_env_block(env: dict[str, str]) -> str:
    pairs = [f"{key}={value}" for key, value in sorted(env.items(), key=lambda item: item[0].upper())]
    return "\0".join(pairs) + "\0\0"


def _kernel32():
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.GetCurrentProcess.argtypes = []
    kernel32.GetCurrentProcess.restype = wintypes.HANDLE
    kernel32.WaitForSingleObject.argtypes = [wintypes.HANDLE, wintypes.DWORD]
    kernel32.WaitForSingleObject.restype = wintypes.DWORD
    kernel32.GetExitCodeProcess.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD)]
    kernel32.GetExitCodeProcess.restype = wintypes.BOOL
    kernel32.TerminateProcess.argtypes = [wintypes.HANDLE, wintypes.UINT]
    kernel32.TerminateProcess.restype = wintypes.BOOL
    kernel32.CreateJobObjectW.argtypes = [wintypes.LPVOID, wintypes.LPCWSTR]
    kernel32.CreateJobObjectW.restype = wintypes.HANDLE
    kernel32.SetInformationJobObject.argtypes = [
        wintypes.HANDLE,
        wintypes.INT,
        wintypes.LPVOID,
        wintypes.DWORD,
    ]
    kernel32.SetInformationJobObject.restype = wintypes.BOOL
    kernel32.AssignProcessToJobObject.argtypes = [wintypes.HANDLE, wintypes.HANDLE]
    kernel32.AssignProcessToJobObject.restype = wintypes.BOOL
    kernel32.TerminateJobObject.argtypes = [wintypes.HANDLE, wintypes.UINT]
    kernel32.TerminateJobObject.restype = wintypes.BOOL
    kernel32.ResumeThread.argtypes = [wintypes.HANDLE]
    kernel32.ResumeThread.restype = wintypes.DWORD
    kernel32.LocalFree.argtypes = [wintypes.LPVOID]
    kernel32.LocalFree.restype = wintypes.LPVOID
    kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
    kernel32.CloseHandle.restype = wintypes.BOOL
    return kernel32


def _advapi32():
    advapi32 = ctypes.WinDLL("advapi32", use_last_error=True)
    advapi32.OpenProcessToken.argtypes = [
        wintypes.HANDLE,
        wintypes.DWORD,
        ctypes.POINTER(wintypes.HANDLE),
    ]
    advapi32.OpenProcessToken.restype = wintypes.BOOL
    advapi32.CreateRestrictedToken.argtypes = [
        wintypes.HANDLE,
        wintypes.DWORD,
        wintypes.DWORD,
        wintypes.LPVOID,
        wintypes.DWORD,
        wintypes.LPVOID,
        wintypes.DWORD,
        wintypes.LPVOID,
        ctypes.POINTER(wintypes.HANDLE),
    ]
    advapi32.CreateRestrictedToken.restype = wintypes.BOOL
    advapi32.ConvertStringSidToSidW.argtypes = [
        wintypes.LPCWSTR,
        ctypes.POINTER(wintypes.LPVOID),
    ]
    advapi32.ConvertStringSidToSidW.restype = wintypes.BOOL
    advapi32.GetLengthSid.argtypes = [wintypes.LPVOID]
    advapi32.GetLengthSid.restype = wintypes.DWORD
    advapi32.SetTokenInformation.argtypes = [
        wintypes.HANDLE,
        wintypes.DWORD,
        wintypes.LPVOID,
        wintypes.DWORD,
    ]
    advapi32.SetTokenInformation.restype = wintypes.BOOL
    advapi32.CreateProcessAsUserW.argtypes = [
        wintypes.HANDLE,
        wintypes.LPCWSTR,
        wintypes.LPWSTR,
        wintypes.LPVOID,
        wintypes.LPVOID,
        wintypes.BOOL,
        wintypes.DWORD,
        wintypes.LPVOID,
        wintypes.LPCWSTR,
        ctypes.POINTER(_STARTUPINFO),
        ctypes.POINTER(_PROCESS_INFORMATION),
    ]
    advapi32.CreateProcessAsUserW.restype = wintypes.BOOL
    return advapi32


def _close_handle(handle: int | None) -> None:
    if handle:
        try:
            _kernel32().CloseHandle(handle)
        except Exception:
            pass


def _last_winerror(function: str) -> OSError:
    code = ctypes.get_last_error()
    return OSError(code, f"{function} failed: {ctypes.FormatError(code)}")


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
