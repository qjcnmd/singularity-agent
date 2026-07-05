from __future__ import annotations

import ctypes
import json
import os
import re
import shutil
import subprocess
import sys as _sys
import time as _time
from contextlib import suppress
from ctypes import wintypes
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, ClassVar

from singularity.observability.redaction import shared_trace_redactor
from singularity.release.paths import resolve_user_data_paths
from singularity.sandbox.models import (
    SandboxRequest,
)
from singularity.sandbox.windows_models import (
    _SANDBOX_IDENTITIES,
    CLEANUP_SCHEMA_VERSION,
    DOCTOR_SCHEMA_VERSION,
    FIREWALL_RULE_GROUP,
    FIREWALL_RULE_NAME,
    LEGACY_FIREWALL_RULE_NAME,
    LEGACY_SANDBOX_ACCOUNT,
    LEGACY_SANDBOX_ACCOUNTS,
    LEGACY_SINGLE_SANDBOX_ACCOUNT,
    LOGIN_UI_USERLIST_KEY,
    OFFLINE_SANDBOX_ACCOUNT,
    ONLINE_SANDBOX_ACCOUNT,
    READINESS_SNAPSHOT_TTL_SECONDS,
    SANDBOX_ACCOUNTS,
    SECURITY_ATTESTATION_KEY,
    SECURITY_ATTESTATION_POLICY,
    SECURITY_ATTESTATION_SCHEMA_VERSION,
    SECURITY_ATTESTATION_VALUE,
    SETUP_SCHEMA_VERSION,
    SETUP_STEP_ORDER,
    WINDOWS_LOCAL_ACCOUNT_NAME_LIMIT,
    WindowsCapabilityState,
    WindowsSandboxCleanupReport,
    WindowsSandboxDoctorReport,
    WindowsSandboxExecution,
    WindowsSandboxPrimitives,
    WindowsSandboxSetup,
    WindowsSandboxSetupReport,
    _sandbox_identity_for_mode,
    _WindowsSandboxIdentity,
)
from singularity.sandbox.windows_platform import is_windows as _is_windows
from singularity.sandbox.windows_runner import (
    WindowsRunnerResult,
    WindowsSandboxRunner,
)
from singularity.utils.serialization import stable_short_hash_text, utc_iso_timestamp

__all__ = [
    "CLEANUP_SCHEMA_VERSION",
    "DOCTOR_SCHEMA_VERSION",
    "FIREWALL_RULE_GROUP",
    "FIREWALL_RULE_NAME",
    "LEGACY_FIREWALL_RULE_NAME",
    "LEGACY_SANDBOX_ACCOUNT",
    "LEGACY_SANDBOX_ACCOUNTS",
    "LEGACY_SINGLE_SANDBOX_ACCOUNT",
    "LOGIN_UI_USERLIST_KEY",
    "OFFLINE_SANDBOX_ACCOUNT",
    "ONLINE_SANDBOX_ACCOUNT",
    "READINESS_SNAPSHOT_TTL_SECONDS",
    "SANDBOX_ACCOUNTS",
    "SECURITY_ATTESTATION_KEY",
    "SECURITY_ATTESTATION_POLICY",
    "SECURITY_ATTESTATION_SCHEMA_VERSION",
    "SECURITY_ATTESTATION_VALUE",
    "SETUP_SCHEMA_VERSION",
    "SETUP_STEP_ORDER",
    "WINDOWS_LOCAL_ACCOUNT_NAME_LIMIT",
    "WINDOWS_PATH_SEPARATOR",
    "_SANDBOX_IDENTITIES",
    "WindowsCapabilityState",
    "WindowsSandboxCleanupReport",
    "WindowsSandboxDoctorReport",
    "WindowsSandboxExecution",
    "WindowsSandboxPrimitives",
    "WindowsSandboxRunner",
    "WindowsSandboxSetup",
    "WindowsSandboxSetupReport",
    "_WindowsSandboxIdentity",
    "_sandbox_identity_for_mode",
]

sys = _sys
time = _time

CRED_TYPE_GENERIC = 1
CRED_PERSIST_LOCAL_MACHINE = 2
NERR_SUCCESS = 0
NERR_USER_EXISTS = 2224
NERR_INVALID_COMPUTER = 2351
ERROR_INVALID_NAME = 123
NERR_USER_NOT_FOUND = 2221
NERR_GROUP_NOT_FOUND = 2220
NERR_INVALID_NAME = 2202
USER_PRIV_USER = 1
UF_SCRIPT = 0x0001
UF_DONT_EXPIRE_PASSWD = 0x10000
# LSA account-right management: CreateProcessWithLogonW requires the target
# account to hold SeInteractiveLogonRight ("Log On Locally"); SE_DENY rights
# override matching allow rights, so deny rights must also be removed.
POLICY_LOOKUP_NAMES = 0x00000800
POLICY_CREATE_ACCOUNT = 0x00000010
SE_INTERACTIVE_LOGON_NAME = "SeInteractiveLogonRight"
SE_BATCH_LOGON_NAME = "SeBatchLogonRight"
SE_NETWORK_LOGON_NAME = "SeNetworkLogonRight"
SE_REMOTE_INTERACTIVE_LOGON_NAME = "SeRemoteInteractiveLogonRight"
SE_SERVICE_LOGON_NAME = "SeServiceLogonRight"
SE_DENY_INTERACTIVE_LOGON_NAME = "SeDenyInteractiveLogonRight"
SE_DENY_BATCH_LOGON_NAME = "SeDenyBatchLogonRight"
SE_DENY_NETWORK_LOGON_NAME = "SeDenyNetworkLogonRight"
SE_DENY_REMOTE_INTERACTIVE_LOGON_NAME = "SeDenyRemoteInteractiveLogonRight"
SE_DENY_SERVICE_LOGON_NAME = "SeDenyServiceLogonRight"
SANDBOX_DENY_LOGON_RIGHTS = (
    SE_DENY_REMOTE_INTERACTIVE_LOGON_NAME,
    SE_DENY_NETWORK_LOGON_NAME,
    SE_DENY_SERVICE_LOGON_NAME,
    SE_DENY_BATCH_LOGON_NAME,
)
SANDBOX_UNNEEDED_ALLOW_LOGON_RIGHTS = (
    SE_REMOTE_INTERACTIVE_LOGON_NAME,
    SE_NETWORK_LOGON_NAME,
    SE_SERVICE_LOGON_NAME,
    SE_BATCH_LOGON_NAME,
)
NERR_MEMBER_IN_GROUP = 2118
ERROR_MEMBER_IN_ALIAS = 1378
ERROR_NOT_FOUND = 1168
STATUS_OBJECT_NAME_NOT_FOUND = 0xC0000034
WINDOWS_PATH_SEPARATOR = ";"





































































































def _available(reason: str, evidence: dict[str, Any]) -> WindowsCapabilityState:
    return WindowsCapabilityState("available", True, reason, evidence)


def _missing(reason: str, evidence: dict[str, Any]) -> WindowsCapabilityState:
    return WindowsCapabilityState("missing", True, reason, evidence)


















def _has_windows_symbols(library: str, *symbols: str) -> bool:
    if not _is_windows():
        return False
    windll = getattr(ctypes, "WinDLL", None)
    if windll is None:
        return False
    try:
        dll = windll(library, use_last_error=True)
        return all(hasattr(dll, symbol) for symbol in symbols)
    except (AttributeError, OSError):
        return False


@dataclass(frozen=True)
class _OperationResult:
    ok: bool
    reason: str = ""
    details: dict[str, Any] = field(default_factory=dict)


class _USER_INFO_1(ctypes.Structure):
    _fields_: ClassVar[list[tuple[str, Any]]] = [
        ("usri1_name", wintypes.LPWSTR),
        ("usri1_password", wintypes.LPWSTR),
        ("usri1_password_age", wintypes.DWORD),
        ("usri1_priv", wintypes.DWORD),
        ("usri1_home_dir", wintypes.LPWSTR),
        ("usri1_comment", wintypes.LPWSTR),
        ("usri1_flags", wintypes.DWORD),
        ("usri1_script_path", wintypes.LPWSTR),
    ]


class _USER_INFO_1003(ctypes.Structure):
    _fields_: ClassVar[list[tuple[str, Any]]] = [("usri1003_password", wintypes.LPWSTR)]


class _CREDENTIALW(ctypes.Structure):
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


class _LSA_UNICODE_STRING(ctypes.Structure):
    _fields_: ClassVar[list[tuple[str, Any]]] = [
        ("Length", wintypes.USHORT),
        ("MaximumLength", wintypes.USHORT),
        ("Buffer", wintypes.LPWSTR),
    ]


class _LSA_OBJECT_ATTRIBUTES(ctypes.Structure):
    _fields_: ClassVar[list[tuple[str, Any]]] = [
        ("Length", wintypes.ULONG),
        ("RootDirectory", wintypes.HANDLE),
        ("ObjectName", ctypes.POINTER(_LSA_UNICODE_STRING)),
        ("Attributes", wintypes.ULONG),
        ("SecurityDescriptor", wintypes.LPVOID),
        ("SecurityQualityOfService", wintypes.LPVOID),
    ]


class _LOCALGROUP_MEMBERS_INFO_0(ctypes.Structure):
    _fields_: ClassVar[list[tuple[str, Any]]] = [("lgrmi0_sid", wintypes.LPVOID)]


def _netapi32():
    dll = ctypes.WinDLL("netapi32", use_last_error=True)
    dll.NetUserAdd.argtypes = [
        wintypes.LPCWSTR,
        wintypes.DWORD,
        wintypes.LPVOID,
        ctypes.POINTER(wintypes.DWORD),
    ]
    dll.NetUserAdd.restype = wintypes.DWORD
    dll.NetUserSetInfo.argtypes = [
        wintypes.LPCWSTR,
        wintypes.LPCWSTR,
        wintypes.DWORD,
        wintypes.LPVOID,
        ctypes.POINTER(wintypes.DWORD),
    ]
    dll.NetUserSetInfo.restype = wintypes.DWORD
    dll.NetUserDel.argtypes = [
        wintypes.LPCWSTR,
        wintypes.LPCWSTR,
    ]
    dll.NetUserDel.restype = wintypes.DWORD
    dll.NetLocalGroupAddMembers.argtypes = [
        wintypes.LPCWSTR,
        wintypes.LPCWSTR,
        wintypes.DWORD,
        wintypes.LPVOID,
        wintypes.DWORD,
    ]
    dll.NetLocalGroupAddMembers.restype = wintypes.DWORD
    return dll


def _advapi32():
    dll = ctypes.WinDLL("advapi32", use_last_error=True)
    dll.CredWriteW.argtypes = [ctypes.POINTER(_CREDENTIALW), wintypes.DWORD]
    dll.CredWriteW.restype = wintypes.BOOL
    dll.CredReadW.argtypes = [
        wintypes.LPCWSTR,
        wintypes.DWORD,
        wintypes.DWORD,
        ctypes.POINTER(ctypes.c_void_p),
    ]
    dll.CredReadW.restype = wintypes.BOOL
    dll.CredDeleteW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD]
    dll.CredDeleteW.restype = wintypes.BOOL
    dll.CredFree.argtypes = [ctypes.c_void_p]
    dll.CredFree.restype = None
    dll.LsaOpenPolicy.argtypes = [
        ctypes.POINTER(_LSA_UNICODE_STRING),
        ctypes.POINTER(_LSA_OBJECT_ATTRIBUTES),
        wintypes.DWORD,
        ctypes.POINTER(wintypes.HANDLE),
    ]
    dll.LsaOpenPolicy.restype = wintypes.ULONG
    dll.LsaClose.argtypes = [wintypes.HANDLE]
    dll.LsaClose.restype = wintypes.ULONG
    dll.LsaAddAccountRights.argtypes = [
        wintypes.HANDLE,
        wintypes.LPVOID,
        ctypes.POINTER(_LSA_UNICODE_STRING),
        wintypes.ULONG,
    ]
    dll.LsaAddAccountRights.restype = wintypes.ULONG
    dll.LsaEnumerateAccountRights.argtypes = [
        wintypes.HANDLE,
        wintypes.LPVOID,
        ctypes.POINTER(ctypes.POINTER(_LSA_UNICODE_STRING)),
        ctypes.POINTER(wintypes.ULONG),
    ]
    dll.LsaEnumerateAccountRights.restype = wintypes.ULONG
    dll.LsaRemoveAccountRights.argtypes = [
        wintypes.HANDLE,
        wintypes.LPVOID,
        wintypes.BOOL,
        ctypes.POINTER(_LSA_UNICODE_STRING),
        wintypes.ULONG,
    ]
    dll.LsaRemoveAccountRights.restype = wintypes.ULONG
    dll.LsaFreeMemory.argtypes = [wintypes.LPVOID]
    dll.LsaFreeMemory.restype = wintypes.ULONG
    dll.ConvertStringSidToSidW.argtypes = [
        wintypes.LPCWSTR,
        ctypes.POINTER(wintypes.LPVOID),
    ]
    dll.ConvertStringSidToSidW.restype = wintypes.BOOL
    return dll




















def _is_elevated() -> bool:
    if not _is_windows():
        return False
    try:
        return bool(ctypes.windll.shell32.IsUserAnAdmin())
    except Exception:
        return False




































































def _run_powershell(command: str) -> subprocess.CompletedProcess[str]:
    executable = shutil.which("powershell") or shutil.which("pwsh")
    if executable is None:
        return subprocess.CompletedProcess(["powershell"], 1, "", "PowerShell unavailable")
    return _run_command(
        [
            executable,
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            command,
        ]
    )

def _run_command(
    command: list[str],
    *,
    timeout_seconds: float = 20,
) -> subprocess.CompletedProcess[str]:
    try:
        return subprocess.run(
            command,
            text=True,
            capture_output=True,
            check=False,
            timeout=timeout_seconds,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        return subprocess.CompletedProcess(
            command,
            1,
            "",
            json.dumps(sandbox_exception_diagnostics("subprocess", exc), ensure_ascii=False),
        )








def _safe_output(result: subprocess.CompletedProcess[str]) -> str:
    text = (result.stderr or result.stdout or "").strip()
    return shared_trace_redactor().redact_text(text)[:500] or f"exit {result.returncode}"


def sandbox_exception_diagnostics(operation: str, exc: BaseException) -> dict[str, Any]:
    return _exception_diagnostics(operation, exc, state_dir=_windows_state_dir_path())


def _state_dir_state() -> WindowsCapabilityState:
    path = _windows_state_dir_path()
    if not path.exists():
        return _missing(
            "Windows sandbox machine state directory is missing.",
            _probe_evidence("windows_state_dir_missing", state_dir=path, path=path),
        )
    return _available(
        "Windows sandbox machine state directory is available.",
        _probe_evidence("windows_state_dir", state_dir=path, path=path),
    )


def _windows_state_dir() -> Path:
    path = _windows_state_dir_path()
    path.mkdir(parents=True, exist_ok=True)
    return path


def _cleanup_probe_root(path: Path) -> None:
    state_dir = _windows_state_dir_path().resolve(strict=False)
    candidate = path.resolve(strict=False)
    if not _is_relative_to(candidate, state_dir) or candidate == state_dir:
        return
    if not candidate.exists():
        return
    icacls = shutil.which("icacls")
    attrib = shutil.which("attrib")
    if icacls:
        _run_command(
            [
                icacls,
                str(candidate),
                "/setintegritylevel",
                "(OI)(CI)M",
                "/T",
                "/C",
                "/Q",
            ]
        )
        _run_command([icacls, str(candidate), "/reset", "/T", "/C", "/Q"])
    if attrib:
        _run_command([attrib, "-R", "-S", "-H", str(candidate / "*"), "/S", "/D"])
    with suppress(OSError):
        shutil.rmtree(candidate)


def _windows_state_dir_path() -> Path:
    if _is_windows():
        program_data = os.environ.get("PROGRAMDATA")
        if program_data:
            return Path(program_data) / "Singularity" / "windows-sandbox"
        system_drive = os.environ.get("SYSTEMDRIVE") or "C:"
        return Path(system_drive + "\\ProgramData") / "Singularity" / "windows-sandbox"
    return resolve_user_data_paths().state_dir / "windows-sandbox"


def _probe_evidence(
    operation: str,
    *,
    state_dir: Path | None = None,
    probe_root: Path | None = None,
    path: Path | None = None,
    extra: dict[str, Any] | None = None,
) -> dict[str, Any]:
    evidence: dict[str, Any] = {
        "operation": operation,
        "elevated": _safe_is_elevated(),
    }
    if path is not None:
        evidence["path_hash"] = _hash_path(path)
    if state_dir is not None:
        evidence["state_dir_hash"] = _hash_path(state_dir)
    if probe_root is not None:
        evidence["probe_root_hash"] = _hash_path(probe_root)
    if extra:
        evidence.update(extra)
    return evidence


def _exception_diagnostics(
    operation: str,
    exc: BaseException,
    *,
    state_dir: Path | None = None,
    probe_root: Path | None = None,
    path: Path | None = None,
    extra: dict[str, Any] | None = None,
) -> dict[str, Any]:
    diagnostics = _probe_evidence(
        operation,
        state_dir=state_dir,
        probe_root=probe_root,
        path=path,
        extra=extra,
    )
    diagnostics["error_type"] = type(exc).__name__
    diagnostics["errno"] = getattr(exc, "errno", None)
    diagnostics["winerror"] = getattr(exc, "winerror", None)
    diagnostics["strerror"] = _diagnostic_text(str(getattr(exc, "strerror", "") or str(exc)), state_dir, probe_root, path)
    diagnostics["returncode"] = None
    diagnostics["stdout_summary"] = ""
    diagnostics["stderr_summary"] = diagnostics["strerror"]
    return diagnostics


def _completed_process_diagnostics(
    operation: str,
    result: subprocess.CompletedProcess[str],
    *,
    state_dir: Path | None = None,
    probe_root: Path | None = None,
    path: Path | None = None,
    extra: dict[str, Any] | None = None,
) -> dict[str, Any]:
    embedded = _embedded_subprocess_diagnostics(result.stderr)
    diagnostics = _probe_evidence(
        operation,
        state_dir=state_dir,
        probe_root=probe_root,
        path=path,
        extra=extra,
    )
    diagnostics["returncode"] = result.returncode
    diagnostics["stdout_summary"] = _diagnostic_text(
        str(embedded.get("stdout_summary") or result.stdout or "") if embedded else (result.stdout or ""),
        state_dir,
        probe_root,
        path,
    )
    diagnostics["stderr_summary"] = _diagnostic_text(
        str(embedded.get("stderr_summary") or embedded.get("strerror") or result.stderr or "")
        if embedded
        else (result.stderr or ""),
        state_dir,
        probe_root,
        path,
    )
    diagnostics["errno"] = embedded.get("errno") if embedded else None
    diagnostics["winerror"] = embedded.get("winerror") if embedded else None
    diagnostics["strerror"] = (
        _diagnostic_text(str(embedded.get("strerror") or ""), state_dir, probe_root, path) if embedded else ""
    )
    if embedded and embedded.get("operation"):
        diagnostics["subprocess_operation"] = embedded.get("operation")
    if embedded and embedded.get("error_type"):
        diagnostics["error_type"] = embedded.get("error_type")
    return diagnostics


def _runner_result_summary(
    operation: str,
    result: WindowsRunnerResult,
    *,
    state_dir: Path | None = None,
    probe_root: Path | None = None,
    path: Path | None = None,
    extra: dict[str, Any] | None = None,
) -> dict[str, Any]:
    summary = _probe_evidence(
        operation,
        state_dir=state_dir,
        probe_root=probe_root,
        path=path,
        extra=extra,
    )
    summary.update(
        {
            "returncode": result.exit_code,
            "stdout_summary": _diagnostic_text(result.stdout, state_dir, probe_root, path),
            "stderr_summary": _diagnostic_text(result.stderr, state_dir, probe_root, path),
            "restricted_process": result.metadata.get("restricted_token"),
            "low_integrity": result.metadata.get("low_integrity"),
            "private_desktop": result.metadata.get("private_desktop"),
            "job_object": result.metadata.get("job_object"),
            "job_killed": result.job_killed,
            "network_denied_verified": result.network_denied_verified,
            "runner_error_code": result.metadata.get("error_code"),
            "runner_error_type": result.metadata.get("error_type"),
            "account_sid_hash": result.metadata.get("account_sid_hash"),
        }
    )
    return summary


def _runner_result_operation(prefix: str, result: WindowsRunnerResult) -> str:
    error_code = str(result.metadata.get("error_code") or "")
    error_type = str(result.metadata.get("error_type") or "")
    stderr = result.stderr.lower()
    if error_code == "runner_result_missing":
        return f"{prefix}_result_write_missing"
    if "createprocesswithlogonw" in stderr:
        return f"{prefix}_create_process_with_logon"
    if "createprocessasuserw" in stderr:
        return f"{prefix}_create_process_as_user"
    if "createrestrictedtoken" in stderr:
        return f"{prefix}_restricted_token"
    if "settokeninformation" in stderr or "low integrity" in stderr:
        return f"{prefix}_low_integrity"
    if "createdesktopw" in stderr:
        return f"{prefix}_private_desktop"
    if "assignprocesstojobobject" in stderr:
        return f"{prefix}_job_object"
    if result.exit_code not in {0, None}:
        return f"{prefix}_child_exit_nonzero"
    if error_type:
        return f"{prefix}_runner_error"
    return prefix


def _is_create_process_with_logon_access_denied(exc: BaseException) -> bool:
    text = str(exc).lower()
    return "createprocesswithlogonw" in text and (
        getattr(exc, "winerror", None) == 5
        or getattr(exc, "errno", None) in {5, 13}
        or "access is denied" in text
        or "拒绝访问" in text
    )


def _account_runner_launch_exception_diagnostics(
    prefix: str,
    exc: BaseException,
    *,
    state_dir: Path,
    probe_root: Path,
    path: Path,
    extra: dict[str, Any] | None = None,
) -> dict[str, Any]:
    if _is_create_process_with_logon_access_denied(exc):
        evidence_extra = {"target": "working_directory"}
        if extra:
            evidence_extra.update(extra)
        return _exception_diagnostics(
            f"{prefix}_working_directory_access",
            exc,
            state_dir=state_dir,
            probe_root=probe_root,
            path=path,
            extra=evidence_extra,
        )
    return _exception_diagnostics(
        _runner_exception_operation(prefix, exc),
        exc,
        state_dir=state_dir,
        probe_root=probe_root,
        path=path,
        extra=extra,
    )


def _runner_exception_operation(prefix: str, exc: BaseException) -> str:
    text = str(exc).lower()
    if "createprocesswithlogonw" in text:
        return f"{prefix}_create_process_with_logon"
    if "createprocessasuserw" in text:
        return f"{prefix}_create_process_as_user"
    if "createrestrictedtoken" in text:
        return f"{prefix}_restricted_token"
    if "settokeninformation" in text or "low integrity" in text:
        return f"{prefix}_low_integrity"
    if "createdesktopw" in text:
        return f"{prefix}_private_desktop"
    if "assignprocesstojobobject" in text:
        return f"{prefix}_job_object"
    return f"{prefix}_launch"


def _probe_failure_runner_result(diagnostics: dict[str, Any]) -> WindowsRunnerResult:
    return WindowsRunnerResult(
        exit_code=None,
        stdout="",
        stderr=str(diagnostics.get("strerror") or diagnostics.get("error_type") or "probe failed"),
        timed_out=False,
        started_at=_now(),
        ended_at=_now(),
        duration_ms=0,
        output_truncated=False,
        job_killed=False,
        network_denied_verified=False,
        metadata={
            "error_code": diagnostics.get("operation"),
            "error_type": diagnostics.get("error_type"),
            "diagnostics": diagnostics,
        },
    )


def _safe_is_elevated() -> bool:
    try:
        return bool(_is_elevated())
    except Exception:
        return False


def _embedded_subprocess_diagnostics(text: str | None) -> dict[str, Any] | None:
    if not text:
        return None
    try:
        payload = json.loads(text)
    except json.JSONDecodeError:
        return None
    if not isinstance(payload, dict) or "operation" not in payload:
        return None
    return payload


def _diagnostic_text(
    text: str,
    state_dir: Path | None = None,
    probe_root: Path | None = None,
    path: Path | None = None,
) -> str:
    sanitized = shared_trace_redactor().redact_text(str(text).strip())
    for item in (state_dir, probe_root, path):
        if item is None:
            continue
        path_text = str(item)
        resolved_text = str(item.expanduser().resolve(strict=False))
        replacement = f"<path:{_hash_path(item)}>"
        candidates = {path_text, resolved_text, path_text.replace("\\", "/"), resolved_text.replace("\\", "/")}
        candidates.update({candidate.replace("\\", "\\\\") for candidate in list(candidates)})
        for candidate in candidates:
            if candidate:
                sanitized = sanitized.replace(candidate, replacement)
    sanitized = re.sub(
        r"(?i)\b[A-Z]:[\\/][^\s\"'<>|]+",
        lambda match: f"<path:{_hash_text(match.group(0))}>",
        sanitized,
    )
    for account in (*SANDBOX_ACCOUNTS, *LEGACY_SANDBOX_ACCOUNTS):
        sanitized = sanitized.replace(account, f"<account:{_hash_text(account)}>")
    sanitized = re.sub(r"\bS-\d(?:-\d+){2,}\b", lambda match: f"<sid:{_hash_text(match.group(0))}>", sanitized)
    return sanitized[:500]


_hash_text = stable_short_hash_text


def _hash_path(value: Path) -> str:
    return _hash_text(str(value.expanduser().resolve(strict=False)))


def _hash_sid(value: str) -> str:
    return _hash_text(value) if value else ""











def _external_writable_paths(request: SandboxRequest) -> list[Path]:
    workspace = Path(request.profile.filesystem.workspace_root).expanduser().resolve(strict=False)
    external: list[Path] = []
    for value in request.profile.filesystem.writable_paths:
        raw = Path(value).expanduser()
        candidate = raw if raw.is_absolute() else workspace / raw
        resolved = candidate.resolve(strict=False)
        if not _is_relative_to(resolved, workspace):
            external.append(resolved)
    return external


def _is_relative_to(child: Path, parent: Path) -> bool:
    try:
        child_key = os.path.normcase(os.path.normpath(str(child)))
        parent_key = os.path.normcase(os.path.normpath(str(parent)))
        return os.path.commonpath([child_key, parent_key]) == parent_key
    except ValueError:
        return False


def _limit_output(stdout: str, stderr: str, max_chars: int | None) -> tuple[str, str, bool]:
    if max_chars is None or len(stdout) + len(stderr) <= max_chars:
        return stdout, stderr, False
    stdout_budget = min(len(stdout), max_chars)
    stderr_budget = max(0, max_chars - stdout_budget)
    return stdout[:stdout_budget], stderr[:stderr_budget], True


_now = utc_iso_timestamp
