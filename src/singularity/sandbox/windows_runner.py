from __future__ import annotations

import argparse
import ctypes
import json
import os
import re
import shutil
import subprocess
import sys
import time
from contextlib import ExitStack, suppress
from ctypes import wintypes
from dataclasses import asdict, dataclass, field, replace
from datetime import UTC, datetime
from pathlib import Path
from typing import Any, BinaryIO, ClassVar

CREATE_NEW_PROCESS_GROUP = 0x00000200
CREATE_NEW_CONSOLE = 0x00000010
JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE = 0x00002000
JobObjectExtendedLimitInformation = 9
CREATE_NO_WINDOW = 0x08000000
CREATE_SUSPENDED = 0x00000004
CREATE_UNICODE_ENVIRONMENT = 0x00000400
STARTF_USESTDHANDLES = 0x00000100
TOKEN_ASSIGN_PRIMARY = 0x0001
TOKEN_DUPLICATE = 0x0002
TOKEN_IMPERSONATE = 0x0004
TOKEN_QUERY = 0x0008
TOKEN_ADJUST_DEFAULT = 0x0080
TOKEN_ADJUST_SESSIONID = 0x0100
DISABLE_MAX_PRIVILEGE = 0x0001
LUA_TOKEN = 0x0004
SE_GROUP_INTEGRITY = 0x00000020
TOKEN_INTEGRITY_LEVEL = 25
WAIT_OBJECT_0 = 0x00000000
WAIT_TIMEOUT = 0x00000102
STILL_ACTIVE = 259
INFINITE = 0xFFFFFFFF
DESKTOP_CREATEWINDOW = 0x0002
DESKTOP_READOBJECTS = 0x0001
DESKTOP_WRITEOBJECTS = 0x0080
DESKTOP_SWITCHDESKTOP = 0x0100
DESKTOP_ACCESS = (
    DESKTOP_CREATEWINDOW | DESKTOP_READOBJECTS | DESKTOP_WRITEOBJECTS | DESKTOP_SWITCHDESKTOP
)
SANDBOX_LOGON_FLAGS = 0
CRED_TYPE_GENERIC = 1
# Token / identity introspection (Level-1 account process proves its own identity
# so doctor can verify the launch was not an admin-current-user fallback).
TokenUser = 1
# Error-mode flags inherited by the Level-2 sandboxed child: suppress Windows
# hard-error / WER dialogs when a sandboxed executable fails to initialize
# (e.g. a tool whose DLLs cannot init under a restricted low-integrity token).
# The launch failure is still reported via the runner result; only the popup
# is suppressed.
SEM_FAILCRITICALERRORS = 0x0001
SEM_NOGPFAULTERRORBOX = 0x0002
SEM_NOOPENFILEERRORBOX = 0x8000
CHILD_ERROR_MODE = (
    SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX
)

DEFAULT_ACCOUNT_NAME = "SingularityOffline"
DEFAULT_CREDENTIAL_TARGET = "SingularityOffline"
NETWORK_PROBE_ENDPOINTS = (("1.1.1.1", 53), ("1.1.1.1", 443), ("8.8.8.8", 53))
SECRET_KEY_RE = re.compile(
    r"(authorization|cookie|token|api[_-]?key|secret|password|private[_-]?key|"
    r"credential|passphrase|access[_-]?token|refresh[_-]?token|client[_-]?secret|"
    r"database[_-]?url|dsn|conn(?:ection)?[_-]?(?:str|string)|openai_api_key|"
    r"anthropic_api_key|github_token|npm_token)",
    re.IGNORECASE,
)
SAFE_BOOLEAN_STATUS_KEYS = {"restricted_token"}
SAFE_NUMERIC_METRIC_KEYS = {
    "input_tokens",
    "output_tokens",
    "total_tokens",
    "cached_input_tokens",
    "reasoning_tokens",
    "prompt_tokens",
    "completion_tokens",
}
ENV_SECRET_RE = re.compile(
    r"(?im)^([A-Z0-9_]*(?:TOKEN|KEY|SECRET|PASSWORD|DSN|CONN_STR|CONN_STRING|CONNECTION_STRING)|"
    r"DATABASE_URL|OPENAI_API_KEY|ANTHROPIC_API_KEY|GITHUB_TOKEN|NPM_TOKEN)\s*=\s*([^\r\n]+)"
)
HEADER_SECRET_RE = re.compile(r"(?im)\b(Authorization|Cookie)\s*:\s*([^\r\n,\]]+)")
CLI_SECRET_FLAG_RE = re.compile(
    r"(?i)(--?(?:password|passwd|pwd|token|secret|api[-_]?key|authorization|cookie)(?:=|\s+))"
    r"('[^']*'|\"[^\"]*\"|[^\s,\]\}]+)"
)
URL_QUERY_SECRET_RE = re.compile(
    r"(?i)([?&](?:access[_-]?token|api[_-]?key|token|secret|password|signature|sig|auth|key)=)"
    r"([^&#\s,\]\}]+)"
)
JSON_ARG_SECRET_RE = re.compile(
    r"(?i)(\"--?(?:password|passwd|pwd|token|secret|api[-_]?key|authorization|cookie)\"\s*,\s*)"
    r"\"[^\"]*\""
)
PRIVATE_KEY_RE = re.compile(
    r"-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----",
    re.IGNORECASE | re.DOTALL,
)
TOKEN_VALUE_RE = re.compile(
    r"\b("
    r"sk-[A-Za-z0-9._\-]+"
    r"|gh[pousr]_[A-Za-z0-9_]+"
    r"|npm_[A-Za-z0-9_]+"
    r"|AKIA[0-9A-Z]{16}"
    r"|eyJ[A-Za-z0-9_-]+\.eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+"
    r"|xox[baprs]-[A-Za-z0-9-]+"
    r"|sk_live_[A-Za-z0-9]+"
    r"|AIza[0-9A-Za-z_-]{35}"
    r")\b"
)


def _is_windows() -> bool:
    return getattr(os, "name", "") == "nt"


@dataclass(frozen=True)
class WindowsRunnerSpec:
    command: list[str] | str
    cwd: str
    env: dict[str, str]
    timeout_seconds: float | None = None
    max_output_chars: int | None = None
    network_mode: str = "denied"
    result_path: str = ""
    operation: str = "command"

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> WindowsRunnerSpec:
        command = payload.get("command")
        if not isinstance(command, list | str):
            raise ValueError("runner spec command must be a list or string.")
        env = payload.get("env") or {}
        if not isinstance(env, dict):
            raise ValueError("runner spec env must be an object.")
        return cls(
            command=[str(item) for item in command] if isinstance(command, list) else str(command),
            cwd=str(payload.get("cwd") or "."),
            env={str(key): str(value) for key, value in env.items()},
            timeout_seconds=float(payload["timeout_seconds"])
            if payload.get("timeout_seconds") is not None
            else None,
            max_output_chars=int(payload["max_output_chars"])
            if payload.get("max_output_chars") is not None
            else None,
            network_mode=str(payload.get("network_mode") or "denied"),
            result_path=str(payload.get("result_path") or ""),
            operation=str(payload.get("operation") or "command"),
        )

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True)
class WindowsRunnerResult:
    exit_code: int | None
    stdout: str
    stderr: str
    timed_out: bool
    started_at: str
    ended_at: str
    duration_ms: int
    output_truncated: bool = False
    job_killed: bool = False
    network_denied_verified: bool = False
    metadata: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> WindowsRunnerResult:
        return cls(
            exit_code=payload.get("exit_code"),
            stdout=str(payload.get("stdout") or ""),
            stderr=str(payload.get("stderr") or ""),
            timed_out=bool(payload.get("timed_out")),
            started_at=str(payload.get("started_at") or ""),
            ended_at=str(payload.get("ended_at") or ""),
            duration_ms=int(payload.get("duration_ms") or 0),
            output_truncated=bool(payload.get("output_truncated")),
            job_killed=bool(payload.get("job_killed")),
            network_denied_verified=bool(payload.get("network_denied_verified")),
            metadata=dict(payload.get("metadata") or {}),
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "exit_code": self.exit_code,
            "stdout": _redact_text(self.stdout),
            "stderr": _redact_text(self.stderr),
            "timed_out": self.timed_out,
            "started_at": self.started_at,
            "ended_at": self.ended_at,
            "duration_ms": self.duration_ms,
            "output_truncated": self.output_truncated,
            "job_killed": self.job_killed,
            "network_denied_verified": self.network_denied_verified,
            "metadata": _redact_value(self.metadata),
        }


class WindowsSandboxRunner:
    """Launches windows_runner.py as the sandbox account for one prepared run."""

    def __init__(
        self,
        *,
        account_name: str = DEFAULT_ACCOUNT_NAME,
        credential_target: str = DEFAULT_CREDENTIAL_TARGET,
        python_executable: str | None = None,
    ) -> None:
        self.account_name = account_name
        self.credential_target = credential_target
        self.python_executable = python_executable or sys.executable

    @staticmethod
    def _materialize_runner_script(sandbox_root: Path) -> Path:
        """Copy windows_runner.py into the ACL'd sandbox_root.

        The sandbox account cannot read windows_runner.py from the host repo
        (the repo lives under the user's private profile, which does not grant
        the sandbox account read access). The runner module is self-contained
        (stdlib only), so materializing a copy into the per-run sandbox_root --
        which is ACL'd to the account -- lets the account process read it
        without leaving a persistent ACL on the host repo. A unique filename is
        used so a non-elevated doctor can create the file fresh instead of
        overwriting a copy a prior elevated run left with a restrictive owner.
        """
        source = Path(__file__).resolve()
        dest = sandbox_root / f"windows_runner_{os.getpid()}_{time.time_ns()}.py"
        dest.write_text(source.read_text(encoding="utf-8"), encoding="utf-8")
        return dest

    def run(self, prepared: Any) -> WindowsRunnerResult:
        if not _is_windows():
            return run_spec(_spec_from_prepared(prepared))
        spec_path = Path(prepared.baseline["runner_spec"])
        result_path = Path(prepared.baseline["runner_result"])
        account_name = str(prepared.baseline.get("sandbox_account") or self.account_name)
        credential_target = str(
            prepared.baseline.get("credential_target") or self.credential_target
        )
        username, password = _read_generic_credential(credential_target)
        account_stdout = ""
        account_stderr = ""
        account_process_spawn_time = 0.0
        account_timed_out = False
        process: _WindowsChildProcess | None = None
        try:
            runner_script = self._materialize_runner_script(Path(prepared.sandbox_root))
            phase_started = time.perf_counter()
            process = _start_account_process(
                [
                    self.python_executable,
                    str(runner_script),
                    "--spec",
                    str(spec_path),
                ],
                cwd=prepared.sandbox_root,
                env=_launcher_env(),
                username=username or account_name,
                password=password,
            )
            account_process_spawn_time = time.perf_counter() - phase_started
            try:
                timeout = prepared.request.profile.resources.timeout_seconds
                # Enforce a finite default wait so a sandboxed command that fails
                # to initialize under the restricted low-integrity token (e.g. a
                # tool whose DLLs cannot init) cannot hang the runner forever;
                # the profile timeout is used when explicitly set.
                wait_timeout = (float(timeout) + 10) if timeout is not None else 40.0
                process.wait(timeout=wait_timeout)
            except subprocess.TimeoutExpired:
                account_timed_out = True
                process.kill()
                with suppress(subprocess.TimeoutExpired):
                    process.wait(timeout=2)
            account_stdout = _process_stdout_text(process)
            account_stderr = _process_stderr_text(process)
        finally:
            _close_child_resources(process)
            password = ""
        if not result_path.exists():
            now = _now()
            return WindowsRunnerResult(
                exit_code=None,
                stdout=account_stdout,
                stderr=(
                    "Windows sandbox runner did not write a result file."
                    + (f"\n{account_stderr}" if account_stderr else "")
                ),
                timed_out=account_timed_out,
                started_at=now,
                ended_at=now,
                duration_ms=0,
                metadata={
                    "error_code": "account_runner_timeout"
                    if account_timed_out
                    else "runner_result_missing"
                },
            )
        phase_started = time.perf_counter()
        result = WindowsRunnerResult.from_dict(json.loads(result_path.read_text(encoding="utf-8")))
        result_import_time = time.perf_counter() - phase_started
        timing = dict(result.metadata.get("timing") or {})
        timing["account_process_spawn_time_seconds"] = account_process_spawn_time
        timing["process_spawn_time_seconds"] = (
            float(timing.get("process_spawn_time_seconds") or 0.0)
            + account_process_spawn_time
        )
        timing["result_import_time_seconds"] = result_import_time
        return replace(result, metadata={**result.metadata, "timing": timing})


def run_spec(spec: WindowsRunnerSpec) -> WindowsRunnerResult:
    if spec.operation == "workspace_cleanup":
        return _run_workspace_cleanup(spec)
    started = time.perf_counter()
    started_at = _now()
    output_truncated = False
    process: _WindowsChildProcess | subprocess.Popen[bytes] | None = None
    timed_out = False
    job_killed = False
    timing: dict[str, float] = {}
    try:
        phase_started = time.perf_counter()
        process = _start_restricted_child(spec)
        timing["process_spawn_time_seconds"] = time.perf_counter() - phase_started
        phase_started = time.perf_counter()
        while True:
            if process.poll() is not None:
                break
            if spec.timeout_seconds is not None and time.perf_counter() - started > spec.timeout_seconds:
                timed_out = True
                job_killed = _terminate_child(process)
                break
            time.sleep(0.02)
        try:
            process.wait(timeout=1)
        except subprocess.TimeoutExpired:
            job_killed = _terminate_child(process) or job_killed
        timing["command_runtime_time_seconds"] = time.perf_counter() - phase_started
        phase_started = time.perf_counter()
        stdout, stderr, output_truncated = _child_output(process, spec.max_output_chars)
        timing["output_collection_time_seconds"] = time.perf_counter() - phase_started
        network_denied_verified = _verify_network_denied(spec)
        account_name, account_sid = _current_process_identity()
        return WindowsRunnerResult(
            exit_code=None if timed_out else process.returncode,
            stdout=stdout,
            stderr=stderr,
            timed_out=timed_out,
            started_at=started_at,
            ended_at=_now(),
            duration_ms=int((time.perf_counter() - started) * 1000),
            output_truncated=output_truncated,
            job_killed=job_killed,
            network_denied_verified=network_denied_verified,
            metadata={
                "restricted_token": _supports_restricted_token(),
                "low_integrity": _supports_low_integrity(),
                "private_desktop": _supports_private_desktop()
                and bool(getattr(process, "private_desktop", False)),
                "job_object": bool(getattr(process, "job_assigned", False)),
                "pid": getattr(process, "pid", None),
                "account_name": account_name,
                "account_sid_hash": _hash_text(account_sid) if account_sid else "",
                "timing": timing,
            },
        )
    except Exception as exc:
        if process is not None and process.poll() is None:
            _terminate_child(process)
        phase_started = time.perf_counter()
        stdout, stderr, output_truncated = _child_output(process, spec.max_output_chars)
        timing["output_collection_time_seconds"] = time.perf_counter() - phase_started
        return WindowsRunnerResult(
            exit_code=None,
            stdout=stdout,
            stderr=(stderr + "\n" if stderr else "") + str(exc),
            timed_out=timed_out,
            started_at=started_at,
            ended_at=_now(),
            duration_ms=int((time.perf_counter() - started) * 1000),
            output_truncated=output_truncated,
            job_killed=job_killed,
            network_denied_verified=False,
            metadata={"error_type": type(exc).__name__, "timing": timing},
        )
    finally:
        _close_child_resources(process)


def _run_workspace_cleanup(spec: WindowsRunnerSpec) -> WindowsRunnerResult:
    started = time.perf_counter()
    started_at = _now()
    try:
        if not isinstance(spec.command, list) or not spec.command:
            raise ValueError("workspace cleanup requires a target path argument.")
        target = Path(spec.command[0])
        root = Path(spec.cwd)
        target_key = os.path.normcase(os.path.abspath(str(target)))
        root_key = os.path.normcase(os.path.abspath(str(root)))
        if os.path.commonpath([target_key, root_key]) != root_key:
            raise ValueError("refusing cleanup outside sandbox root")
        if target.name.lower() != "workspace":
            raise ValueError("refusing cleanup of non-workspace target")
        if target.exists():
            _clear_cleanup_attributes(target)
            shutil.rmtree(target)
        account_name, account_sid = _current_process_identity()
        return WindowsRunnerResult(
            exit_code=0,
            stdout="workspace cleanup complete\n",
            stderr="",
            timed_out=False,
            started_at=started_at,
            ended_at=_now(),
            duration_ms=int((time.perf_counter() - started) * 1000),
            output_truncated=False,
            job_killed=False,
            network_denied_verified=True,
            metadata={
                "operation": "workspace_cleanup",
                "restricted_token": False,
                "low_integrity": False,
                "private_desktop": False,
                "job_object": False,
                "account_name": account_name,
                "account_sid_hash": _hash_text(account_sid) if account_sid else "",
            },
        )
    except Exception as exc:
        return WindowsRunnerResult(
            exit_code=1,
            stdout="",
            stderr=str(exc),
            timed_out=False,
            started_at=started_at,
            ended_at=_now(),
            duration_ms=int((time.perf_counter() - started) * 1000),
            output_truncated=False,
            job_killed=False,
            network_denied_verified=False,
            metadata={
                "operation": "workspace_cleanup",
                "error_type": type(exc).__name__,
            },
        )


def _clear_cleanup_attributes(target: Path) -> None:
    paths = [target]
    with suppress(OSError):
        paths.extend(sorted(target.rglob("*"), key=lambda p: len(p.parts), reverse=True))
    for path in paths:
        with suppress(OSError):
            os.chmod(path, 0o700 if path.is_dir() else 0o600)


def _start_restricted_child(spec: WindowsRunnerSpec) -> _WindowsChildProcess | subprocess.Popen[bytes]:
    if _is_windows():
        return _start_windows_restricted_child(spec)
    if isinstance(spec.command, list):
        command: str | list[str] = spec.command
        shell = False
    else:
        command = spec.command
        shell = True
    return subprocess.Popen(
        command,
        cwd=spec.cwd,
        env=spec.env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        shell=shell,
        start_new_session=True,
    )


def _assign_to_kill_on_close_job(process_handle: int) -> tuple[int | None, bool]:
    if not _is_windows():
        return None, False
    kernel32 = _kernel32()
    job = kernel32.CreateJobObjectW(None, None)
    if not job:
        return None, False
    info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION()
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
    ok = kernel32.SetInformationJobObject(
        job,
        JobObjectExtendedLimitInformation,
        ctypes.byref(info),
        ctypes.sizeof(info),
    )
    if ok:
        ok = kernel32.AssignProcessToJobObject(job, process_handle)
    if not ok:
        kernel32.CloseHandle(job)
        return None, False
    return job, True


def _terminate_child(process: _WindowsChildProcess | subprocess.Popen[bytes]) -> bool:
    kill = getattr(process, "kill", None)
    if callable(kill):
        kill()
        return bool(getattr(process, "job_assigned", False))
    return False


def _close_child_resources(process: Any) -> None:
    close_handles = getattr(process, "_close_handles", None)
    if callable(close_handles):
        close_handles()


def _verify_network_denied(spec: WindowsRunnerSpec) -> bool:
    if spec.network_mode != "denied":
        return True
    if _is_windows():
        endpoints = json.dumps(NETWORK_PROBE_ENDPOINTS)
        probe_spec = WindowsRunnerSpec(
            command=[
                sys.executable,
                "-c",
                (
                    "import json, socket\n"
                    f"endpoints=json.loads({endpoints!r})\n"
                    "for host, port in endpoints:\n"
                    "    s=socket.socket(); s.settimeout(1)\n"
                    "    try:\n"
                    "        s.connect((host, int(port)))\n"
                    "    except OSError:\n"
                    "        continue\n"
                    "    finally:\n"
                    "        s.close()\n"
                    "    raise SystemExit(7)\n"
                    "raise SystemExit(0)\n"
                ),
            ],
            cwd=spec.cwd,
            env=spec.env,
            timeout_seconds=3,
            max_output_chars=1000,
            network_mode="allowed",
        )
        probe = _start_restricted_child(probe_spec)
        deadline = time.perf_counter() + 3
        while probe.poll() is None and time.perf_counter() < deadline:
            time.sleep(0.02)
        if probe.poll() is None:
            _terminate_child(probe)
        try:
            probe.wait(timeout=1)
        except subprocess.TimeoutExpired:
            _terminate_child(probe)
        finally:
            _close_child_resources(probe)
        return probe.returncode == 0
    code = (
        "import socket; "
        "s=socket.socket(); s.settimeout(1); "
        "\ntry:\n s.connect(('1.1.1.1', 53)); print('NETWORK_ALLOWED'); raise SystemExit(7)\n"
        "except Exception:\n raise SystemExit(0)\n"
    )
    try:
        completed = subprocess.run(
            [sys.executable, "-c", code],
            cwd=spec.cwd,
            env=spec.env,
            text=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=3,
            check=False,
        )
    except Exception:
        return False
    return completed.returncode == 0


def _child_output(
    process: _WindowsChildProcess | subprocess.Popen[bytes] | None,
    max_chars: int | None,
) -> tuple[str, str, bool]:
    if process is None:
        return "", "", False
    stdout_reader = getattr(process, "stdout_text", None)
    stderr_reader = getattr(process, "stderr_text", None)
    if callable(stdout_reader) and callable(stderr_reader):
        stdout = str(stdout_reader())
        stderr = str(stderr_reader())
    else:
        try:
            raw_stdout, raw_stderr = process.communicate(timeout=0.1)
        except Exception:
            raw_stdout, raw_stderr = b"", b""
        stdout = raw_stdout.decode("utf-8", errors="replace")
        stderr = raw_stderr.decode("utf-8", errors="replace")
    return _limit_output(stdout, stderr, max_chars)


def _process_stdout_text(process: Any) -> str:
    reader = getattr(process, "stdout_text", None)
    if callable(reader):
        return str(reader())
    return ""


def _process_stderr_text(process: Any) -> str:
    reader = getattr(process, "stderr_text", None)
    if callable(reader):
        return str(reader())
    return ""


def _read_and_unlink_text(path: Path) -> str:
    try:
        text = path.read_bytes().decode("utf-8", errors="replace") if path.exists() else ""
    finally:
        try:
            path.unlink()
        except FileNotFoundError:
            pass
        except OSError:
            pass
    return text


def _limit_output(stdout: str, stderr: str, max_chars: int | None) -> tuple[str, str, bool]:
    if max_chars is None or len(stdout) + len(stderr) <= max_chars:
        return stdout, stderr, False
    stdout_budget = min(len(stdout), max_chars)
    stderr_budget = max(0, max_chars - stdout_budget)
    return stdout[:stdout_budget], stderr[:stderr_budget], True


def _redact_value(value: Any) -> Any:
    if isinstance(value, dict):
        return {
            key: item
            if _is_safe_redaction_value(key, item)
            else "<redacted>"
            if SECRET_KEY_RE.search(str(key))
            else _redact_value(item)
            for key, item in value.items()
        }
    if isinstance(value, list):
        return [_redact_value(item) for item in value]
    if isinstance(value, tuple):
        return [_redact_value(item) for item in value]
    if isinstance(value, str):
        return _redact_text(value)
    return value


def _redact_text(text: str) -> str:
    redacted = PRIVATE_KEY_RE.sub("<redacted>", text)
    redacted = ENV_SECRET_RE.sub(lambda match: f"{match.group(1)}=<redacted>", redacted)
    redacted = HEADER_SECRET_RE.sub(lambda match: f"{match.group(1)}: <redacted>", redacted)
    redacted = URL_QUERY_SECRET_RE.sub(lambda match: f"{match.group(1)}<redacted>", redacted)
    redacted = JSON_ARG_SECRET_RE.sub(lambda match: f'{match.group(1)}"<redacted>"', redacted)
    redacted = CLI_SECRET_FLAG_RE.sub(lambda match: f"{match.group(1)}<redacted>", redacted)
    redacted = TOKEN_VALUE_RE.sub("<redacted>", redacted)
    return redacted


def _is_safe_redaction_value(key: object, value: Any) -> bool:
    key_text = str(key).lower()
    if key_text in SAFE_BOOLEAN_STATUS_KEYS and isinstance(value, bool):
        return True
    return key_text in SAFE_NUMERIC_METRIC_KEYS and isinstance(value, int | float) and not isinstance(value, bool)


class _WindowsChildProcess:
    def __init__(
        self,
        *,
        command: list[str] | str,
        process_handle: int,
        thread_handle: int,
        process_id: int,
        job_handle: int | None,
        job_assigned: bool,
        desktop_handle: int | None,
        stdout_path: Path,
        stderr_path: Path,
        streams: list[BinaryIO],
    ) -> None:
        self.args = command
        self.pid = process_id
        self.returncode: int | None = None
        self.job_assigned = job_assigned
        self.private_desktop = desktop_handle is not None
        self._process_handle = process_handle
        self._thread_handle = thread_handle
        self._job_handle = job_handle
        self._desktop_handle = desktop_handle
        self._stdout_path = stdout_path
        self._stderr_path = stderr_path
        self._streams = streams
        self._closed = False

    def poll(self) -> int | None:
        if self.returncode is not None:
            return self.returncode
        exit_code = wintypes.DWORD()
        if not _kernel32().GetExitCodeProcess(self._process_handle, ctypes.byref(exit_code)):
            return None
        if exit_code.value == STILL_ACTIVE:
            return None
        self.returncode = int(exit_code.value)
        return self.returncode

    def wait(self, timeout: int | float | None = None) -> int:
        wait_ms = INFINITE if timeout is None else max(1, int(float(timeout) * 1000))
        wait_result = _kernel32().WaitForSingleObject(self._process_handle, wait_ms)
        if wait_result == WAIT_TIMEOUT:
            raise subprocess.TimeoutExpired(self.args, float(timeout or 0))
        if wait_result != WAIT_OBJECT_0:
            raise _last_winerror("WaitForSingleObject")
        exit_code = self.poll()
        self._close_streams()
        self._close_handles()
        self.returncode = int(exit_code if exit_code is not None else 1)
        return self.returncode

    def kill(self) -> None:
        if self.poll() is not None:
            return
        if self._job_handle and self.job_assigned:
            _kernel32().TerminateJobObject(self._job_handle, 1)
        else:
            _kernel32().TerminateProcess(self._process_handle, 1)

    def stdout_text(self) -> str:
        self._close_streams()
        return _read_and_unlink_text(self._stdout_path)

    def stderr_text(self) -> str:
        self._close_streams()
        return _read_and_unlink_text(self._stderr_path)

    def _close_streams(self) -> None:
        for stream in self._streams:
            with suppress(Exception):
                stream.close()
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
        if self._desktop_handle:
            with suppress(Exception):
                _user32().CloseDesktop(self._desktop_handle)


def _start_windows_restricted_child(spec: WindowsRunnerSpec) -> _WindowsChildProcess:
    import msvcrt

    token = _create_restricted_token()
    cwd = Path(spec.cwd)
    stdout_path = cwd / f".singularity-windows-sandbox-{os.getpid()}-{time.time_ns()}.stdout"
    stderr_path = cwd / f".singularity-windows-sandbox-{os.getpid()}-{time.time_ns()}.stderr"
    desktop_name = f"SingularitySandbox-{os.getpid()}-{time.time_ns()}"
    desktop_handle = None
    job_handle = None
    process_handle = None
    thread_handle = None
    child_owns_process_handles = False
    try:
        with ExitStack() as stream_stack:
            stdout_file = stream_stack.enter_context(stdout_path.open("w+b"))
            stderr_file = stream_stack.enter_context(stderr_path.open("w+b"))
            stdin_file = stream_stack.enter_context(Path(os.devnull).open("rb"))
            streams: list[BinaryIO] = [stdout_file, stderr_file, stdin_file]
            for stream in streams:
                _set_handle_inheritable(stream)
            desktop_handle = _create_private_desktop(desktop_name)
            startup = STARTUPINFO()
            startup.cb = ctypes.sizeof(STARTUPINFO)
            startup.lpDesktop = desktop_name
            startup.dwFlags = STARTF_USESTDHANDLES
            startup.hStdInput = msvcrt.get_osfhandle(stdin_file.fileno())
            startup.hStdOutput = msvcrt.get_osfhandle(stdout_file.fileno())
            startup.hStdError = msvcrt.get_osfhandle(stderr_file.fileno())
            process_info = PROCESS_INFORMATION()
            command_line = ctypes.create_unicode_buffer(_windows_command_line(spec.command))
            env_block = ctypes.create_unicode_buffer(_windows_env_block(spec.env))
            flags = CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW | CREATE_SUSPENDED
            ok = _advapi32().CreateProcessAsUserW(
                token,
                None,
                command_line,
                None,
                None,
                True,
                flags,
                env_block,
                _windows_extended_path(cwd),
                ctypes.byref(startup),
                ctypes.byref(process_info),
            )
            if not ok:
                raise _last_winerror("CreateProcessAsUserW")
            process_handle = process_info.hProcess
            thread_handle = process_info.hThread
            job_handle, job_assigned = _assign_to_kill_on_close_job(process_info.hProcess)
            if not job_assigned:
                raise OSError("AssignProcessToJobObject failed")
            if _kernel32().ResumeThread(process_info.hThread) == 0xFFFFFFFF:
                raise _last_winerror("ResumeThread")
            child = _WindowsChildProcess(
                command=spec.command,
                process_handle=process_info.hProcess,
                thread_handle=process_info.hThread,
                process_id=process_info.dwProcessId,
                job_handle=job_handle,
                job_assigned=job_assigned,
                desktop_handle=desktop_handle,
                stdout_path=stdout_path,
                stderr_path=stderr_path,
                streams=streams,
            )
            child_owns_process_handles = True
            stream_stack.pop_all()
        return child
    except Exception:
        if not child_owns_process_handles:
            _close_handle(thread_handle)
            _close_handle(process_handle)
        if job_handle:
            _close_handle(job_handle)
        if desktop_handle:
            with suppress(Exception):
                _user32().CloseDesktop(desktop_handle)
        raise
    finally:
        _close_handle(token)


def _create_restricted_token() -> int:
    token = wintypes.HANDLE()
    restricted = wintypes.HANDLE()
    restricted_handle = None
    restricted_released = False
    access = (
        TOKEN_ASSIGN_PRIMARY
        | TOKEN_DUPLICATE
        | TOKEN_IMPERSONATE
        | TOKEN_QUERY
        | TOKEN_ADJUST_DEFAULT
        | TOKEN_ADJUST_SESSIONID
    )
    if not _advapi32().OpenProcessToken(_kernel32().GetCurrentProcess(), access, ctypes.byref(token)):
        raise _last_winerror("OpenProcessToken")
    try:
        flags = DISABLE_MAX_PRIVILEGE | LUA_TOKEN
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
        restricted_handle = restricted.value
        if not restricted_handle:
            raise OSError("CreateRestrictedToken returned an empty token handle.")
        _set_low_integrity(restricted_handle)
        restricted_released = True
        return restricted_handle
    finally:
        if restricted_handle is not None and not restricted_released:
            _close_handle(restricted_handle)
        _close_handle(token.value)


def _set_low_integrity(token: int) -> None:
    sid = wintypes.LPVOID()
    if not _advapi32().ConvertStringSidToSidW("S-1-16-4096", ctypes.byref(sid)):
        raise _last_winerror("ConvertStringSidToSidW")
    try:
        label = TOKEN_MANDATORY_LABEL()
        label.Label.Attributes = SE_GROUP_INTEGRITY
        label.Label.Sid = sid
        size = ctypes.sizeof(label) + _advapi32().GetLengthSid(sid)
        if not _advapi32().SetTokenInformation(
            token,
            TOKEN_INTEGRITY_LEVEL,
            ctypes.byref(label),
            size,
        ):
            raise _last_winerror("SetTokenInformation")
    finally:
        _kernel32().LocalFree(sid)


def _create_private_desktop(name: str) -> int:
    handle = _user32().CreateDesktopW(name, None, None, 0, DESKTOP_ACCESS, None)
    if not handle:
        raise _last_winerror("CreateDesktopW")
    return int(handle)


def _windows_command_line(command: list[str] | str) -> str:
    if isinstance(command, str):
        comspec = os.environ.get("COMSPEC") or "cmd.exe"
        return subprocess.list2cmdline([comspec, "/d", "/s", "/c", command])
    return subprocess.list2cmdline([str(part) for part in command])


def _windows_env_block(env: dict[str, str]) -> str:
    pairs = [f"{key}={value}" for key, value in sorted(env.items(), key=lambda item: item[0].upper())]
    return "\0".join(pairs) + "\0\0"


def _set_handle_inheritable(stream: BinaryIO) -> None:
    import msvcrt

    os.set_handle_inheritable(msvcrt.get_osfhandle(stream.fileno()), True)


def _windows_extended_path(path: Path) -> str:
    value = str(path)
    if value.startswith("\\\\?\\"):
        return value
    if value.startswith("\\\\"):
        return "\\\\?\\UNC\\" + value[2:]
    if re.match(r"^[A-Za-z]:[\\/]", value):
        value = value[:2] + re.sub(r"[\\/]+", r"\\", value[2:])
        return "\\\\?\\" + value
    if path.is_absolute():
        return "\\\\?\\" + value
    return value


def _spec_from_prepared(prepared: Any) -> WindowsRunnerSpec:
    request = prepared.request
    return WindowsRunnerSpec(
        command=request.command,
        cwd=str(prepared.execution_cwd),
        env=prepared.env,
        timeout_seconds=request.profile.resources.timeout_seconds,
        max_output_chars=request.profile.resources.max_output_chars,
        network_mode=request.profile.network.mode.value,
        result_path=str(prepared.baseline.get("runner_result") or ""),
    )


def _launcher_env() -> dict[str, str]:
    env: dict[str, str] = {}
    for name in (
        "COMSPEC",
        "PATH",
        "PATHEXT",
        "PYTHONIOENCODING",
        "SYSTEMDRIVE",
        "SYSTEMROOT",
        "TEMP",
        "TMP",
        "WINDIR",
    ):
        value = os.environ.get(name)
        if value is not None:
            env[name] = value
    env.setdefault("PYTHONIOENCODING", "utf-8")
    return env


def _start_account_process(
    command: list[str],
    *,
    cwd: Path,
    env: dict[str, str],
    username: str,
    password: str,
) -> _WindowsChildProcess:
    stdout_path = cwd / "account-runner.stdout"
    stderr_path = cwd / "account-runner.stderr"
    process_info = PROCESS_INFORMATION()
    try:
        import msvcrt

        with ExitStack() as stream_stack:
            stdout_file = stream_stack.enter_context(stdout_path.open("w+b"))
            stderr_file = stream_stack.enter_context(stderr_path.open("w+b"))
            stdin_file = stream_stack.enter_context(Path(os.devnull).open("rb"))
            streams: list[BinaryIO] = [stdout_file, stderr_file, stdin_file]
            for stream in streams:
                _set_handle_inheritable(stream)
            startup = STARTUPINFO()
            startup.cb = ctypes.sizeof(STARTUPINFO)
            startup.dwFlags = STARTF_USESTDHANDLES
            startup.hStdInput = msvcrt.get_osfhandle(stdin_file.fileno())
            startup.hStdOutput = msvcrt.get_osfhandle(stdout_file.fileno())
            startup.hStdError = msvcrt.get_osfhandle(stderr_file.fileno())
            command_line = ctypes.create_unicode_buffer(_windows_command_line(command))
            env_block = ctypes.create_unicode_buffer(_windows_env_block(env))
            ok = _advapi32().CreateProcessWithLogonW(
                username,
                ".",
                password,
                SANDBOX_LOGON_FLAGS,
                None,
                command_line,
                CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
                env_block,
                str(cwd),
                ctypes.byref(startup),
                ctypes.byref(process_info),
            )
            if not ok:
                raise _last_winerror("CreateProcessWithLogonW")
            child = _WindowsChildProcess(
                command=command,
                process_handle=process_info.hProcess,
                thread_handle=process_info.hThread,
                process_id=process_info.dwProcessId,
                job_handle=None,
                job_assigned=False,
                desktop_handle=None,
                stdout_path=stdout_path,
                stderr_path=stderr_path,
                streams=streams,
            )
            stream_stack.pop_all()
        return child
    except Exception:
        if process_info.hProcess:
            _close_handle(process_info.hProcess)
        if process_info.hThread:
            _close_handle(process_info.hThread)
        raise


def _read_generic_credential(target: str) -> tuple[str, str]:
    credential_ptr = ctypes.c_void_p()
    if not _advapi32().CredReadW(target, CRED_TYPE_GENERIC, 0, ctypes.byref(credential_ptr)):
        raise _last_winerror("CredReadW")
    try:
        credential = ctypes.cast(credential_ptr, ctypes.POINTER(CREDENTIALW)).contents
        username = credential.UserName or ""
        blob = ctypes.string_at(credential.CredentialBlob, credential.CredentialBlobSize)
        try:
            password = blob.decode("utf-16-le")
        except UnicodeDecodeError:
            password = blob.decode("utf-8", errors="replace")
        return username, password.rstrip("\x00")
    finally:
        _advapi32().CredFree(credential_ptr)


def _supports_restricted_token() -> bool:
    return _has_symbol("advapi32", "CreateRestrictedToken")


def _supports_low_integrity() -> bool:
    return _has_symbol("advapi32", "ConvertStringSidToSidW") and _has_symbol(
        "advapi32", "SetTokenInformation"
    )


def _supports_private_desktop() -> bool:
    return _has_symbol("user32", "CreateDesktopW")


def _has_symbol(library: str, symbol: str) -> bool:
    if not _is_windows():
        return False
    try:
        return hasattr(ctypes.WinDLL(library, use_last_error=True), symbol)
    except Exception:
        return False


def _hash_text(value: str) -> str:
    import hashlib

    return hashlib.sha256(value.encode("utf-8")).hexdigest()[:16]


def _current_process_identity() -> tuple[str, str]:
    """Return (account_name, sid_string) for the current process token.

    The Level-1 account process runs as the sandbox account, so its token user
    SID is the proof that CreateProcessWithLogonW did not silently fall back to
    the admin current user. Called inside the account-launched runner so the raw
    SID never crosses the process boundary; only its hash is written to disk.
    """
    if not _is_windows():
        return "", ""
    advapi32 = _advapi32()
    token = wintypes.HANDLE()
    if not advapi32.OpenProcessToken(
        _kernel32().GetCurrentProcess(), TOKEN_QUERY, ctypes.byref(token)
    ):
        return "", ""
    try:
        length = wintypes.DWORD(0)
        advapi32.GetTokenInformation(token, TokenUser, None, 0, ctypes.byref(length))
        if not length.value:
            return "", ""
        buffer = (ctypes.c_byte * length.value)()
        if not advapi32.GetTokenInformation(
            token, TokenUser, buffer, length.value, ctypes.byref(length)
        ):
            return "", ""
        user = ctypes.cast(buffer, ctypes.POINTER(TOKEN_USER)).contents
        sid_ptr = ctypes.c_void_p()
        if not advapi32.ConvertSidToStringSidW(user.User.Sid, ctypes.byref(sid_ptr)) or not sid_ptr.value:
            return "", ""
        try:
            sid_string = ctypes.wstring_at(sid_ptr.value)
        except Exception:
            return "", ""
        finally:
            _kernel32().LocalFree(sid_ptr.value)
        name_buffer = ctypes.create_unicode_buffer(256)
        name_len = wintypes.DWORD(256)
        name = (
            name_buffer.value or ""
            if advapi32.GetUserNameW(name_buffer, ctypes.byref(name_len))
            else ""
        )
        return name, sid_string
    finally:
        _close_handle(token.value)


def _now() -> str:
    return datetime.now(UTC).isoformat()


class JOBOBJECT_BASIC_LIMIT_INFORMATION(ctypes.Structure):
    _fields_: ClassVar[list[tuple[str, Any]]] = [
        ("PerProcessUserTimeLimit", ctypes.c_int64),
        ("PerJobUserTimeLimit", ctypes.c_int64),
        ("LimitFlags", ctypes.c_uint32),
        ("MinimumWorkingSetSize", ctypes.c_size_t),
        ("MaximumWorkingSetSize", ctypes.c_size_t),
        ("ActiveProcessLimit", ctypes.c_uint32),
        ("Affinity", ctypes.c_size_t),
        ("PriorityClass", ctypes.c_uint32),
        ("SchedulingClass", ctypes.c_uint32),
    ]


class IO_COUNTERS(ctypes.Structure):
    _fields_: ClassVar[list[tuple[str, Any]]] = [
        ("ReadOperationCount", ctypes.c_uint64),
        ("WriteOperationCount", ctypes.c_uint64),
        ("OtherOperationCount", ctypes.c_uint64),
        ("ReadTransferCount", ctypes.c_uint64),
        ("WriteTransferCount", ctypes.c_uint64),
        ("OtherTransferCount", ctypes.c_uint64),
    ]


class JOBOBJECT_EXTENDED_LIMIT_INFORMATION(ctypes.Structure):
    _fields_: ClassVar[list[tuple[str, Any]]] = [
        ("BasicLimitInformation", JOBOBJECT_BASIC_LIMIT_INFORMATION),
        ("IoInfo", IO_COUNTERS),
        ("ProcessMemoryLimit", ctypes.c_size_t),
        ("JobMemoryLimit", ctypes.c_size_t),
        ("PeakProcessMemoryUsed", ctypes.c_size_t),
        ("PeakJobMemoryUsed", ctypes.c_size_t),
    ]


class SECURITY_ATTRIBUTES(ctypes.Structure):
    _fields_: ClassVar[list[tuple[str, Any]]] = [
        ("nLength", wintypes.DWORD),
        ("lpSecurityDescriptor", wintypes.LPVOID),
        ("bInheritHandle", wintypes.BOOL),
    ]


class STARTUPINFO(ctypes.Structure):
    _fields_: ClassVar[list[tuple[str, Any]]] = [
        ("cb", wintypes.DWORD),
        ("lpReserved", wintypes.LPWSTR),
        ("lpDesktop", wintypes.LPWSTR),
        ("lpTitle", wintypes.LPWSTR),
        ("dwX", wintypes.DWORD),
        ("dwY", wintypes.DWORD),
        ("dwXSize", wintypes.DWORD),
        ("dwYSize", wintypes.DWORD),
        ("dwXCountChars", wintypes.DWORD),
        ("dwYCountChars", wintypes.DWORD),
        ("dwFillAttribute", wintypes.DWORD),
        ("dwFlags", wintypes.DWORD),
        ("wShowWindow", wintypes.WORD),
        ("cbReserved2", wintypes.WORD),
        ("lpReserved2", ctypes.POINTER(ctypes.c_byte)),
        ("hStdInput", wintypes.HANDLE),
        ("hStdOutput", wintypes.HANDLE),
        ("hStdError", wintypes.HANDLE),
    ]


class PROCESS_INFORMATION(ctypes.Structure):
    _fields_: ClassVar[list[tuple[str, Any]]] = [
        ("hProcess", wintypes.HANDLE),
        ("hThread", wintypes.HANDLE),
        ("dwProcessId", wintypes.DWORD),
        ("dwThreadId", wintypes.DWORD),
    ]


class SID_AND_ATTRIBUTES(ctypes.Structure):
    _fields_: ClassVar[list[tuple[str, Any]]] = [
        ("Sid", wintypes.LPVOID),
        ("Attributes", wintypes.DWORD),
    ]


class TOKEN_MANDATORY_LABEL(ctypes.Structure):
    _fields_: ClassVar[list[tuple[str, Any]]] = [("Label", SID_AND_ATTRIBUTES)]


class CREDENTIALW(ctypes.Structure):
    _fields_: ClassVar[list[tuple[str, Any]]] = [
        ("Flags", wintypes.DWORD),
        ("Type", wintypes.DWORD),
        ("TargetName", wintypes.LPWSTR),
        ("Comment", wintypes.LPWSTR),
        ("LastWritten", wintypes.FILETIME),
        ("CredentialBlobSize", wintypes.DWORD),
        ("CredentialBlob", ctypes.POINTER(ctypes.c_ubyte)),
        ("Persist", wintypes.DWORD),
        ("AttributeCount", wintypes.DWORD),
        ("Attributes", wintypes.LPVOID),
        ("TargetAlias", wintypes.LPWSTR),
        ("UserName", wintypes.LPWSTR),
    ]


class TOKEN_USER(ctypes.Structure):
    _fields_: ClassVar[list[tuple[str, Any]]] = [("User", SID_AND_ATTRIBUTES)]


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
    kernel32.SetErrorMode.argtypes = [wintypes.UINT]
    kernel32.SetErrorMode.restype = wintypes.UINT
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
        ctypes.POINTER(STARTUPINFO),
        ctypes.POINTER(PROCESS_INFORMATION),
    ]
    advapi32.CreateProcessAsUserW.restype = wintypes.BOOL
    advapi32.CreateProcessWithLogonW.argtypes = [
        wintypes.LPCWSTR,
        wintypes.LPCWSTR,
        wintypes.LPCWSTR,
        wintypes.DWORD,
        wintypes.LPCWSTR,
        wintypes.LPWSTR,
        wintypes.DWORD,
        wintypes.LPVOID,
        wintypes.LPCWSTR,
        ctypes.POINTER(STARTUPINFO),
        ctypes.POINTER(PROCESS_INFORMATION),
    ]
    advapi32.CreateProcessWithLogonW.restype = wintypes.BOOL
    advapi32.CredReadW.argtypes = [
        wintypes.LPCWSTR,
        wintypes.DWORD,
        wintypes.DWORD,
        ctypes.POINTER(ctypes.c_void_p),
    ]
    advapi32.CredReadW.restype = wintypes.BOOL
    advapi32.CredFree.argtypes = [ctypes.c_void_p]
    advapi32.CredFree.restype = None
    advapi32.ConvertSidToStringSidW.argtypes = [
        wintypes.LPVOID,
        ctypes.POINTER(ctypes.c_void_p),
    ]
    advapi32.ConvertSidToStringSidW.restype = wintypes.BOOL
    advapi32.GetTokenInformation.argtypes = [
        wintypes.HANDLE,
        wintypes.DWORD,
        wintypes.LPVOID,
        wintypes.DWORD,
        ctypes.POINTER(wintypes.DWORD),
    ]
    advapi32.GetTokenInformation.restype = wintypes.BOOL
    advapi32.GetUserNameW.argtypes = [
        wintypes.LPWSTR,
        ctypes.POINTER(wintypes.DWORD),
    ]
    advapi32.GetUserNameW.restype = wintypes.BOOL
    return advapi32


def _user32():
    user32 = ctypes.WinDLL("user32", use_last_error=True)
    user32.CreateDesktopW.argtypes = [
        wintypes.LPCWSTR,
        wintypes.LPCWSTR,
        wintypes.LPVOID,
        wintypes.DWORD,
        wintypes.DWORD,
        ctypes.POINTER(SECURITY_ATTRIBUTES),
    ]
    user32.CreateDesktopW.restype = wintypes.HANDLE
    user32.CloseDesktop.argtypes = [wintypes.HANDLE]
    user32.CloseDesktop.restype = wintypes.BOOL
    return user32


def _close_handle(handle: int | None) -> None:
    if handle:
        with suppress(Exception):
            _kernel32().CloseHandle(handle)


def _last_winerror(function: str) -> OSError:
    code = ctypes.get_last_error()
    return OSError(code, f"{function} failed: {ctypes.FormatError(code)}")


def main(argv: list[str] | None = None) -> int:
    # This process is the account-launched Level-1 runner. Suppress Windows
    # hard-error / WER dialogs so the Level-2 sandboxed child (CreateProcessAsUserW
    # with a restricted low-integrity token) does not pop up "application was
    # unable to start correctly" dialogs when a tool's DLLs fail to initialize
    # under the restricted token. The error mode is inherited by the child.
    if _is_windows():
        _kernel32().SetErrorMode(CHILD_ERROR_MODE)
    parser = argparse.ArgumentParser()
    parser.add_argument("--spec", required=True)
    args = parser.parse_args(argv)
    spec_path = Path(args.spec)
    spec = WindowsRunnerSpec.from_dict(json.loads(spec_path.read_text(encoding="utf-8")))
    result = run_spec(spec)
    result_path = Path(spec.result_path) if spec.result_path else spec_path.with_name("runner-result.json")
    result_path.write_text(json.dumps(result.to_dict(), ensure_ascii=False, indent=2), encoding="utf-8")
    return 0 if result.exit_code == 0 and not result.timed_out else 1


if __name__ == "__main__":
    raise SystemExit(main())
