from __future__ import annotations

import importlib.util
import json
import os
import shutil
import sys
import sysconfig
from contextlib import suppress
from pathlib import Path
from types import SimpleNamespace
from typing import Any

from singularity.sandbox.models import SandboxResourceLimits
from singularity.sandbox.windows_common import (
    SANDBOX_ACCOUNTS,
    WindowsCapabilityState,
    WindowsSandboxRunner,
    _account_runner_launch_exception_diagnostics,
    _cleanup_probe_root,
    _completed_process_diagnostics,
    _diagnostic_text,
    _exception_diagnostics,
    _hash_path,
    _hash_sid,
    _hash_text,
    _is_windows,
    _missing,
    _OperationResult,
    _probe_evidence,
    _probe_failure_runner_result,
    _run_command,
    _runner_result_operation,
    _runner_result_summary,
    _windows_state_dir,
    _windows_state_dir_path,
    _WindowsSandboxIdentity,
)
from singularity.sandbox.windows_runner import WindowsRunnerResult, WindowsRunnerSpec


def _account_name_diagnostics(*args, **kwargs):
    from singularity.sandbox.windows_identity import _account_name_diagnostics as impl

    return impl(*args, **kwargs)


def _account_sid(*args, **kwargs):
    from singularity.sandbox.windows_identity import _account_sid as impl

    return impl(*args, **kwargs)


def _apply_probe_root_acl(*args, **kwargs):
    from singularity.sandbox.windows_acl import _apply_probe_root_acl as impl

    return impl(*args, **kwargs)


def _credential_state(*args, **kwargs):
    from singularity.sandbox.windows_doctor import _credential_state as impl

    return impl(*args, **kwargs)


def _runner_state(*args, **kwargs):
    from singularity.sandbox.windows_doctor import _runner_state as impl

    return impl(*args, **kwargs)


def _state_from_bool(*args, **kwargs):
    from singularity.sandbox.windows_doctor import _state_from_bool as impl

    return impl(*args, **kwargs)


def _runner_smoke_state(identity: _WindowsSandboxIdentity) -> WindowsCapabilityState:
    if not _is_windows():
        return _missing("Windows runner smoke requires Windows.", {"runner": "windows_runner.py"})
    runner = _runner_state()
    if not runner.ready:
        return runner
    sid = _account_sid(identity.account_name)
    if not _credential_state(identity).ready or not sid:
        state_dir = _windows_state_dir_path()
        return _missing(
            "Windows runner smoke requires sandbox account and credential.",
            _probe_evidence(
                "runner_smoke_prerequisites",
                state_dir=state_dir,
                probe_root=state_dir / "runner-smoke",
                extra={"runner": "windows_runner.py"},
            ),
        )
    state_dir = _windows_state_dir_path()
    root = state_dir / "runner-smoke"
    try:
        state_dir = _windows_state_dir()
        root = state_dir / "runner-smoke"
        root.mkdir(parents=True, exist_ok=True)
        acl = _apply_probe_root_acl(
            root,
            account_names=(identity.account_name,),
            operation="runner_smoke_acl",
        )
        if not acl.ok:
            return _missing(
                "Windows runner smoke ACL setup failed.",
                {
                    **_probe_evidence("runner_smoke_acl", state_dir=state_dir, probe_root=root),
                    "runner": "windows_runner.py",
                    "reason": acl.reason,
                    "details": acl.details,
                },
            )
        spec_path = root / "runner-spec.json"
        result_path = root / "runner-result.json"
        spec = WindowsRunnerSpec(
            command=[sys.executable, "-c", "print('sandbox-smoke')"],
            cwd=str(root),
            env=_runtime_env({}),
            timeout_seconds=5,
            max_output_chars=2000,
            network_mode="allowed",
            result_path=str(result_path),
        )
        try:
            spec_path.write_text(json.dumps(spec.to_dict(), ensure_ascii=False), encoding="utf-8")
        except OSError as exc:
            return _missing(
                "Windows runner smoke spec could not be written.",
                _exception_diagnostics(
                    "runner_smoke_spec_write",
                    exc,
                    state_dir=state_dir,
                    probe_root=root,
                    path=spec_path,
                ),
            )
        prepared = SimpleNamespace(
            sandbox_root=root,
            baseline={
                "runner_spec": str(spec_path),
                "runner_result": str(result_path),
                "sandbox_account": identity.account_name,
                "credential_target": identity.credential_target,
                "sandbox_role": identity.role,
            },
            request=SimpleNamespace(profile=SimpleNamespace(resources=SandboxResourceLimits(timeout_seconds=5))),
        )
        try:
            result = WindowsSandboxRunner(
                account_name=identity.account_name,
                credential_target=identity.credential_target,
            ).run(prepared)
        except Exception as exc:
            return _missing(
                "Windows account-backed runner smoke failed.",
                _account_runner_launch_exception_diagnostics(
                    "runner_smoke",
                    exc,
                    state_dir=state_dir,
                    probe_root=root,
                    path=root,
                ),
            )
        account_sid_hash = result.metadata.get("account_sid_hash")
        account_identity_verified = bool(account_sid_hash) and account_sid_hash == _hash_sid(sid)
        ready = (
            result.exit_code == 0
            and "sandbox-smoke" in result.stdout
            and bool(result.metadata.get("restricted_token"))
            and bool(result.metadata.get("low_integrity"))
            and bool(result.metadata.get("private_desktop"))
            and bool(result.metadata.get("job_object"))
            and account_identity_verified
        )
        return _state_from_bool(
            ready,
            "Windows account-backed runner smoke passed.",
            "Windows account-backed runner smoke failed.",
            _runner_result_summary(
                _runner_result_operation("runner_smoke", result),
                result,
                state_dir=state_dir,
                probe_root=root,
                path=root,
                extra={"account_identity_verified": account_identity_verified},
            ),
        )
    except Exception as exc:
        return _missing(
            "Windows account-backed runner smoke failed.",
            _account_runner_launch_exception_diagnostics(
                "runner_smoke",
                exc,
                state_dir=state_dir,
                probe_root=root,
                path=root,
            ),
        )
    finally:
        _cleanup_probe_root(root)

def _account_python_smoke(
    *,
    identity: _WindowsSandboxIdentity,
    cwd: Path,
    code: str,
    timeout_seconds: int,
    operation_prefix: str = "account_python_smoke",
) -> WindowsRunnerResult:
    spec_path = cwd / "runner-spec.json"
    result_path = cwd / "runner-result.json"
    for path in (spec_path, result_path):
        with suppress(FileNotFoundError):
            path.unlink()
    spec = WindowsRunnerSpec(
        command=[sys.executable, "-c", code],
        cwd=str(cwd),
        env=_runtime_env({}),
        timeout_seconds=timeout_seconds,
        max_output_chars=2000,
        network_mode="allowed",
        result_path=str(result_path),
    )
    state_dir = _windows_state_dir_path()
    try:
        spec_path.write_text(json.dumps(spec.to_dict(), ensure_ascii=False), encoding="utf-8")
    except OSError as exc:
        return _probe_failure_runner_result(
            _exception_diagnostics(
                f"{operation_prefix}_spec_write",
                exc,
                state_dir=state_dir,
                probe_root=cwd,
                path=spec_path,
            )
        )
    prepared = SimpleNamespace(
        sandbox_root=cwd,
        baseline={
            "runner_spec": str(spec_path),
            "runner_result": str(result_path),
            "sandbox_account": identity.account_name,
            "credential_target": identity.credential_target,
            "sandbox_role": identity.role,
        },
        request=SimpleNamespace(
            profile=SimpleNamespace(resources=SandboxResourceLimits(timeout_seconds=timeout_seconds))
        ),
    )
    try:
        return WindowsSandboxRunner(
            account_name=identity.account_name,
            credential_target=identity.credential_target,
        ).run(prepared)
    except Exception as exc:
        return _probe_failure_runner_result(
            _account_runner_launch_exception_diagnostics(
                operation_prefix,
                exc,
                state_dir=state_dir,
                probe_root=cwd,
                path=cwd,
            )
        )

def _python_runtime_smoke_diagnostics(
    identities: tuple[_WindowsSandboxIdentity, ...],
) -> tuple[dict[str, Any], ...]:
    if not _is_windows():
        return ()
    state_dir = _windows_state_dir_path()
    if not state_dir.exists():
        return ()
    root = state_dir / "python-runtime-smoke"
    try:
        root = _windows_state_dir() / "python-runtime-smoke"
        root.mkdir(parents=True, exist_ok=True)
        acl = _apply_probe_root_acl(
            root,
            account_names=tuple(identity.account_name for identity in identities),
            operation="python_runtime_smoke_acl",
        )
        if not acl.ok:
            return (
                {
                    "kind": "python_runtime_environment_blocker",
                    "status": "blocked",
                    "failure_type": "probe_acl_setup_failed",
                    "reason": "Python runtime smoke ACL setup failed.",
                    "evidence": {
                        **_probe_evidence("python_runtime_smoke_acl", state_dir=state_dir, probe_root=root),
                        "details": acl.details,
                    },
                },
            )
        diagnostics: list[dict[str, Any]] = []
        for identity in identities:
            sid = _account_sid(identity.account_name)
            cwd = root / identity.role
            cwd.mkdir(parents=True, exist_ok=True)
            role_acl = _apply_probe_root_acl(
                cwd,
                account_names=(identity.account_name,),
                operation=f"python_runtime_smoke_{identity.role}_acl",
            )
            if not role_acl.ok:
                diagnostics.append(
                    {
                        "kind": "python_runtime_environment_blocker",
                        "status": "blocked",
                        "failure_type": "role_probe_acl_setup_failed",
                        "reason": "Python runtime smoke role directory ACL setup failed.",
                        "evidence": {
                            **_probe_evidence(
                                f"python_runtime_smoke_{identity.role}_acl",
                                state_dir=state_dir,
                                probe_root=root,
                                path=cwd,
                                extra={
                                    "sandbox_role": identity.role,
                                    "network_mode": identity.network_mode.value,
                                    "target": "probe_root_acl",
                                },
                            ),
                            "details": role_acl.details,
                        },
                    }
                )
                continue
            result = _account_python_smoke(
                identity=identity,
                cwd=cwd,
                code=_PYTHON_RUNTIME_SMOKE_CODE,
                timeout_seconds=5,
                operation_prefix="python_runtime_smoke",
            )
            if result.exit_code == 0:
                continue
            diagnostics.append(
                _python_runtime_smoke_diagnostic(
                    identity=identity,
                    sid=sid,
                    result=result,
                    state_dir=state_dir,
                    root=root,
                )
            )
        return tuple(diagnostics)
    except Exception as exc:
        return (
            {
                "kind": "python_runtime_environment_blocker",
                "status": "blocked",
                "failure_type": "probe_execution_failed",
                "reason": "Python runtime smoke failed before module import checks completed.",
                "evidence": _exception_diagnostics(
                    "python_runtime_smoke",
                    exc,
                    state_dir=state_dir,
                    probe_root=root,
                    path=root,
                ),
            },
        )
    finally:
        _cleanup_probe_root(root)

def _python_runtime_smoke_diagnostic(
    *,
    identity: _WindowsSandboxIdentity,
    sid: str,
    result: WindowsRunnerResult,
    state_dir: Path,
    root: Path,
) -> dict[str, Any]:
    payload = _python_runtime_smoke_payload(result.stdout)
    module_status = _python_runtime_module_status(payload)
    failure_type, module = _python_runtime_failure(payload, result)
    evidence = _runner_result_summary(
        _runner_result_operation("python_runtime_smoke", result),
        result,
        state_dir=state_dir,
        probe_root=root,
        path=root,
        extra={
            "role": identity.role,
            "network_mode": identity.network_mode.value,
            "account": _account_name_diagnostics(identity.account_name),
            "account_sid_hash": _hash_sid(sid),
            "module_status": module_status,
            "failure_type": failure_type,
            "module": module,
            "sandbox_role": identity.role,
            "restricted_token": result.metadata.get("restricted_token"),
            "low_integrity": result.metadata.get("low_integrity"),
            "private_desktop": result.metadata.get("private_desktop"),
            "job_object": result.metadata.get("job_object"),
            "runtime_target_hashes": _runtime_target_hashes(),
            "runtime_access": _diagnostic_payload(
                payload.get("runtime_access") if isinstance(payload.get("runtime_access"), dict) else {}
            ),
            "ssl": _diagnostic_payload(payload.get("ssl") if isinstance(payload.get("ssl"), dict) else {}),
        },
    )
    return {
        "kind": "python_runtime_environment_blocker",
        "status": "blocked",
        "failure_type": failure_type,
        "module": module,
        "sandbox_role": identity.role,
        "restricted_token": result.metadata.get("restricted_token"),
        "low_integrity": result.metadata.get("low_integrity"),
        "private_desktop": result.metadata.get("private_desktop"),
        "job_object": result.metadata.get("job_object"),
        "reason": "Sandbox account Python runtime smoke failed.",
        "evidence": evidence,
    }

def _python_runtime_smoke_payload(stdout: str) -> dict[str, Any]:
    try:
        payload = json.loads(stdout.strip().splitlines()[-1])
    except (IndexError, json.JSONDecodeError):
        return {}
    if not isinstance(payload, dict):
        return {}
    return payload

def _python_runtime_module_status(payload: dict[str, Any]) -> dict[str, str]:
    modules = payload.get("modules") if isinstance(payload.get("modules"), dict) else payload
    if not isinstance(modules, dict):
        return {}
    result: dict[str, str] = {}
    for name in ("_ctypes", "ctypes", "_ssl", "ssl", "socket", "hashlib", "pathlib"):
        value = modules.get(name)
        if isinstance(value, dict):
            result[name] = str(value.get("status") or "unknown")
        elif isinstance(value, str):
            result[name] = value
    return result

def _python_runtime_failure(
    payload: dict[str, Any],
    result: WindowsRunnerResult,
) -> tuple[str, str]:
    modules_payload = payload.get("modules")
    modules: dict[str, Any] = modules_payload if isinstance(modules_payload, dict) else {}
    runtime_access_payload = payload.get("runtime_access")
    runtime_access: dict[str, Any] = runtime_access_payload if isinstance(runtime_access_payload, dict) else {}
    output = f"{result.stdout}\n{result.stderr}\n{_python_runtime_payload_text(payload)}".lower()
    low_integrity_failed_modules = [
        name for name in ("_ctypes", "ctypes", "_ssl", "ssl") if _module_failed(modules, name)
    ]
    generic_c_extension_failures = [
        name for name in low_integrity_failed_modules if name not in {"_ssl", "ssl"}
    ]
    if generic_c_extension_failures and _looks_like_dll_initialization_failed(output):
        return (
            "python_c_extension_low_integrity_runtime_initialization_failed",
            generic_c_extension_failures[0],
        )
    if _module_failed(modules, "_ssl"):
        if "dll search path" in output:
            return "dll_search_path_failed", "_ssl"
        if "libssl" in output or "libcrypto" in output:
            return "openssl_dependency_dll_load_failed", "_ssl"
        if _looks_like_dll_initialization_failed(output):
            return "ssl_low_integrity_runtime_initialization_failed", "_ssl"
        return "_ssl.pyd_load_failed", "_ssl"
    if _module_failed(modules, "ssl"):
        if _access_failed(runtime_access, ("openssl_config", "openssl_providers")):
            return "openssl_provider_or_config_unreadable", "ssl"
        if _access_failed(runtime_access, ("certificate_paths",)):
            return "certificate_path_unreadable", "ssl"
        return "_ssl.pyd_load_failed", "ssl"
    if _access_failed(runtime_access, ("openssl_config", "openssl_providers")):
        return "openssl_provider_or_config_unreadable", "ssl"
    if _access_failed(runtime_access, ("certificate_paths",)):
        return "certificate_path_unreadable", "ssl"
    if _access_failed(runtime_access, ("temp", "tmp", "profile")):
        return "temp_or_profile_access_failed", "ssl"
    if "dll search path" in output:
        return "dll_search_path_failed", "_ssl"
    if "libssl" in output or "libcrypto" in output:
        return "openssl_dependency_dll_load_failed", "_ssl"
    if _looks_like_dll_initialization_failed(output):
        return "ssl_low_integrity_runtime_initialization_failed", "_ssl"
    if "_ssl" in output or "_ssl.pyd" in output:
        return "_ssl.pyd_load_failed", "_ssl"
    return "ssl_low_integrity_runtime_initialization_failed", "ssl"

def _module_failed(modules: dict[str, Any], name: str) -> bool:
    value = modules.get(name)
    if isinstance(value, dict):
        return str(value.get("status") or "").lower() == "failed"
    return str(value or "").lower() == "failed"

def _access_failed(runtime_access: dict[str, Any], names: tuple[str, ...]) -> bool:
    for name in names:
        value = runtime_access.get(name)
        if isinstance(value, dict):
            status = str(value.get("status") or "").lower()
            if status in {"failed", "missing"}:
                return True
    return False

def _looks_like_dll_initialization_failed(output: str) -> bool:
    return any(
        marker in output
        for marker in (
            "dll initialization",
            "initialization routine failed",
            "初始化例程失败",
            "出现了内部错误",
        )
    )

def _python_runtime_payload_text(value: Any) -> str:
    if isinstance(value, dict):
        return "\n".join(_python_runtime_payload_text(item) for item in value.values())
    if isinstance(value, list | tuple):
        return "\n".join(_python_runtime_payload_text(item) for item in value)
    if isinstance(value, str):
        return value
    return ""

def _runtime_target_hashes() -> list[str]:
    return [_hash_path(path) for path, _permission in _runner_runtime_acl_targets()]

def _diagnostic_payload(value: Any) -> Any:
    if isinstance(value, dict):
        return {str(key): _diagnostic_payload(item) for key, item in value.items()}
    if isinstance(value, list | tuple):
        return [_diagnostic_payload(item) for item in value]
    if isinstance(value, str):
        return _diagnostic_text(value)
    return value

_PYTHON_RUNTIME_SMOKE_CODE = r"""
import importlib
import json
import os
from pathlib import Path


def _hash_path(path):
    import hashlib

    return hashlib.sha256(str(Path(path).expanduser()).encode("utf-8")).hexdigest()[:16]


def _check_readable(path):
    if not path:
        return {"status": "missing"}
    try:
        target = Path(path)
        if not target.exists():
            return {"status": "missing", "path_hash": _hash_path(target)}
        if target.is_dir():
            next(iter(target.iterdir()), None)
        else:
            with target.open("rb") as handle:
                handle.read(1)
        return {"status": "passed", "path_hash": _hash_path(target)}
    except BaseException as exc:
        return {
            "status": "failed",
            "path_hash": _hash_path(path),
            "error_type": type(exc).__name__,
            "message": str(exc)[:200],
        }


def _runtime_roots():
    import sys

    roots = []
    for value in (sys.prefix, sys.base_prefix, sys.exec_prefix):
        if value:
            path = Path(value)
            if path.exists() and path not in roots:
                roots.append(path)
    return roots


def _check_many(paths):
    statuses = {}
    for path in paths:
        checked = _check_readable(path)
        statuses[checked.get("path_hash", _hash_path(path))] = checked["status"]
    return {"status": "failed" if "failed" in statuses.values() else "passed", "entries": statuses}


modules = {}
ok = True
for name in ("_ctypes", "ctypes", "_ssl", "ssl", "socket", "hashlib", "pathlib"):
    try:
        module = importlib.import_module(name)
        state = {"status": "passed"}
        filename = getattr(module, "__file__", "")
        if filename:
            state["file_hash"] = _hash_path(filename)
        modules[name] = state
    except BaseException as exc:
        ok = False
        modules[name] = {
            "status": "failed",
            "error_type": type(exc).__name__,
            "message": str(exc)[:200],
        }

ssl_info = {}
runtime_access = {}
try:
    ssl = importlib.import_module("ssl")
    ssl_info["openssl_version"] = getattr(ssl, "OPENSSL_VERSION", "")
    paths = ssl.get_default_verify_paths()
    ssl_info["default_verify_paths"] = {
        name: _hash_path(value)
        for name, value in {
            "cafile": paths.cafile,
            "capath": paths.capath,
            "openssl_cafile": paths.openssl_cafile,
            "openssl_capath": paths.openssl_capath,
        }.items()
        if value
    }
    cert_status = {}
    for value in (paths.cafile, paths.capath, paths.openssl_cafile, paths.openssl_capath):
        if value:
            cert_status[str(_hash_path(value))] = _check_readable(value)["status"]
    runtime_access["certificate_paths"] = {
        "status": "failed" if "failed" in cert_status.values() else "passed",
        "entries": cert_status,
    }
except BaseException as exc:
    ssl_info["error_type"] = type(exc).__name__
    ssl_info["message"] = str(exc)[:200]

for env_name, key in (("OPENSSL_CONF", "openssl_config"), ("OPENSSL_MODULES", "openssl_providers")):
    runtime_access[key] = _check_readable(os.environ.get(env_name))
for env_name, key in (("TEMP", "temp"), ("TMP", "tmp"), ("USERPROFILE", "profile")):
    runtime_access[key] = _check_readable(os.environ.get(env_name))
openssl_dlls = []
openssl_configs = []
openssl_providers = []
for root in _runtime_roots():
    openssl_dlls.extend((root / "Library" / "bin").glob("libssl*.dll"))
    openssl_dlls.extend((root / "Library" / "bin").glob("libcrypto*.dll"))
    config = root / "Library" / "ssl" / "openssl.cnf"
    if config.exists():
        openssl_configs.append(config)
    openssl_providers.extend((root / "Library" / "lib" / "ossl-modules").glob("*.dll"))
runtime_access["openssl_dlls"] = _check_many(openssl_dlls)
if openssl_configs:
    runtime_access["openssl_config"] = _check_many(openssl_configs)
if openssl_providers:
    runtime_access["openssl_providers"] = _check_many(openssl_providers)
elif runtime_access.get("openssl_providers", {}).get("status") == "missing" and not os.environ.get(
    "OPENSSL_MODULES"
):
    runtime_access["openssl_providers"] = {"status": "not_configured"}

print(json.dumps({"modules": modules, "ssl": ssl_info, "runtime_access": runtime_access}, sort_keys=True))
raise SystemExit(0 if ok else 7)
""".strip()

def _python_runtime_roots() -> tuple[Path, ...]:
    candidates: list[Path] = []
    executable = Path(sys.executable).expanduser().resolve(strict=False)
    if executable:
        candidates.append(executable.parent)
    for value in (
        sys.prefix,
        sys.base_prefix,
        getattr(sys, "exec_prefix", ""),
        sysconfig.get_config_var("base"),
        sysconfig.get_config_var("installed_base"),
        sysconfig.get_config_var("prefix"),
        sysconfig.get_config_var("exec_prefix"),
    ):
        if value:
            candidates.append(Path(str(value)).expanduser().resolve(strict=False))
    return _unique_existing_paths(candidates)

def _unique_existing_paths(paths: list[Path] | tuple[Path, ...]) -> tuple[Path, ...]:
    unique: list[Path] = []
    for path in paths:
        resolved = path.expanduser().resolve(strict=False)
        if resolved.exists() and all(existing != resolved for existing in unique):
            unique.append(resolved)
    return tuple(unique)

def _python_runtime_path_directories() -> tuple[Path, ...]:
    directories: list[Path] = []
    executable = Path(sys.executable).expanduser().resolve(strict=False)
    if executable.parent.exists():
        directories.append(executable.parent)
    for path, _permission in _runner_runtime_acl_targets():
        candidate = path if path.is_dir() else path.parent
        if candidate.exists():
            directories.append(candidate)
    return _unique_existing_paths(directories)

def _runner_runtime_acl_targets() -> tuple[tuple[Path, str], ...]:
    targets: list[tuple[Path, str]] = []

    def add(path: Path, permission: str) -> None:
        resolved = path.expanduser().resolve(strict=False)
        if resolved.exists() and all(existing != resolved for existing, _permission in targets):
            targets.append((resolved, permission))

    executable = Path(sys.executable).expanduser().resolve(strict=False)
    if executable.parent.exists():
        add(executable.parent, "RX")

    roots = _python_runtime_roots()
    for root in roots:
        add(root / "DLLs", "(OI)(CI)RX")
        add(root / "Library" / "bin", "(OI)(CI)RX")
        add(root / "Library" / "ssl", "(OI)(CI)RX")
        add(root / "Library" / "lib" / "ossl-modules", "(OI)(CI)RX")
        for pattern in ("python*.dll",):
            for child in sorted(root.glob(pattern), key=lambda path: path.name.casefold()):
                if child.is_file():
                    add(child, "RX")
        for child in sorted((root / "DLLs").glob("*.pyd"), key=lambda path: path.name.casefold()):
            if child.name.casefold() in {"_ssl.pyd", "_hashlib.pyd", "_socket.pyd"}:
                add(child, "RX")
        for child in sorted((root / "Library" / "bin").glob("*.dll"), key=lambda path: path.name.casefold()):
            lowered = child.name.casefold()
            if lowered.startswith(("libssl", "libcrypto")):
                add(child, "RX")
        openssl_config = root / "Library" / "ssl" / "openssl.cnf"
        if openssl_config.exists():
            add(openssl_config, "RX")
        for provider in sorted(
            (root / "Library" / "lib" / "ossl-modules").glob("*.dll"),
            key=lambda path: path.name.casefold(),
        ):
            if provider.is_file():
                add(provider, "RX")

    for module_name in ("_ssl", "_hashlib", "_socket"):
        with suppress(Exception):
            spec = importlib.util.find_spec(module_name)
            origin = Path(str(spec.origin)).expanduser().resolve(strict=False) if spec and spec.origin else None
            if origin and origin.exists():
                add(origin.parent, "(OI)(CI)RX")
                add(origin, "RX")
    return tuple(targets)

def _runner_runtime_stale_acl_targets() -> tuple[Path, ...]:
    return _python_runtime_roots()

def _ensure_runner_runtime_access(
    account_names: tuple[str, ...] = SANDBOX_ACCOUNTS,
) -> _OperationResult:
    if not _is_windows():
        return _OperationResult(False, "Runner runtime ACL setup requires Windows.")
    icacls = shutil.which("icacls")
    if icacls is None:
        return _OperationResult(False, "icacls is required for runner runtime ACL setup.")
    targets = _runner_runtime_acl_targets()
    details = {
        "runtime_target_hashes": [_hash_path(path) for path, _permission in targets],
        "account_name_hashes": [_hash_text(account) for account in account_names],
    }
    stale_cleanup = _remove_stale_runner_runtime_base_access(
        icacls,
        targets,
        account_names,
        details,
    )
    if stale_cleanup is not None:
        return stale_cleanup
    for path, permission in targets:
        command = [icacls, str(path), "/grant:r"]
        command.extend(f"{account}:{permission}" for account in account_names)
        command.extend(("/C", "/Q"))
        result = _run_command(command, timeout_seconds=120)
        if result.returncode != 0:
            return _OperationResult(
                False,
                "Failed to grant sandbox accounts read/execute access to the Python runtime.",
                _completed_process_diagnostics(
                    "runner_runtime_acl_grant",
                    result,
                    state_dir=_windows_state_dir_path(),
                    path=path,
                    extra=details,
                ),
            )
    return _OperationResult(
        True,
        "runner_runtime_access_ready",
        {"changed": bool(targets and account_names), **details},
    )

def _remove_stale_runner_runtime_base_access(
    icacls: str,
    targets: tuple[tuple[Path, str], ...],
    account_names: tuple[str, ...],
    details: dict[str, Any],
) -> _OperationResult | None:
    del targets
    for path in _runner_runtime_stale_acl_targets():
        command = [icacls, str(path), "/remove:g", *account_names, "/C", "/Q"]
        result = _run_command(command, timeout_seconds=120)
        if result.returncode != 0:
            return _OperationResult(
                False,
                "Failed to remove stale sandbox account access from the Python runtime root.",
                _completed_process_diagnostics(
                    "runner_runtime_acl_stale_cleanup",
                    result,
                    state_dir=_windows_state_dir_path(),
                    path=path,
                    extra=details,
                ),
            )
    return None

def _remove_runner_runtime_access(account_names: tuple[str, ...]) -> _OperationResult:
    if not account_names:
        return _OperationResult(True, "runner_runtime_access_not_present", {"changed": False})
    if not _is_windows():
        return _OperationResult(False, "Runner runtime ACL cleanup requires Windows.")
    icacls = shutil.which("icacls")
    if icacls is None:
        return _OperationResult(False, "icacls is required for runner runtime ACL cleanup.")
    targets = _runner_runtime_acl_targets()
    details = {
        "runtime_target_hashes": [_hash_path(path) for path, _permission in targets],
        "account_name_hashes": [_hash_text(account) for account in account_names],
    }
    cleanup_targets = tuple(dict.fromkeys((*_runner_runtime_stale_acl_targets(), *(path for path, _permission in targets))))
    for path in cleanup_targets:
        command = [icacls, str(path), "/remove:g", *account_names, "/C", "/Q"]
        result = _run_command(command, timeout_seconds=120)
        if result.returncode != 0:
            return _OperationResult(
                False,
                "Failed to remove sandbox account access from the Python runtime.",
                _completed_process_diagnostics(
                    "runner_runtime_acl_cleanup",
                    result,
                    state_dir=_windows_state_dir_path(),
                    path=path,
                    extra=details,
                ),
            )
    return _OperationResult(
        True,
        "runner_runtime_access_removed",
        {"changed": bool(targets), **details},
    )

def _runtime_env(env: dict[str, str]) -> dict[str, str]:
    runtime = dict(env)
    for name in (
        "COMSPEC",
        "PATH",
        "PATHEXT",
        "SYSTEMDRIVE",
        "SYSTEMROOT",
        "TEMP",
        "TMP",
        "WINDIR",
    ):
        value = os.environ.get(name)
        if value is not None and name not in runtime:
            runtime[name] = value
    path_entries = [str(path) for path in _python_runtime_path_directories()]
    existing_path = runtime.get("PATH", "")
    for entry in existing_path.split(os.pathsep):
        if entry and entry not in path_entries:
            path_entries.append(entry)
    if path_entries:
        runtime["PATH"] = os.pathsep.join(path_entries)
    runtime.setdefault("PYTHONIOENCODING", "utf-8")
    return runtime

def _resolve_command(command: list[str] | str, *, env: dict[str, str]) -> list[str] | str:
    if isinstance(command, str):
        return command
    if not command:
        return command
    resolved = [str(part) for part in command]
    executable = resolved[0]
    if Path(executable).is_absolute():
        return resolved
    candidate = _resolve_executable(executable, env)
    if candidate is not None:
        resolved[0] = str(candidate)
    return resolved

def _resolve_executable(name: str, env: dict[str, str]) -> Path | None:
    if not _is_windows():
        found = shutil.which(name, path=env.get("PATH") or os.environ.get("PATH"))
        return Path(found) if found else None
    search_path = env.get("PATH") or os.environ.get("PATH") or ""
    found = shutil.which(name, path=search_path)
    if found:
        return Path(found)
    suffixes = env.get("PATHEXT") or os.environ.get("PATHEXT") or ".COM;.EXE;.BAT;.CMD"
    if Path(name).suffix:
        return None
    for suffix in suffixes.split(";"):
        found = shutil.which(f"{name}{suffix}", path=search_path)
        if found:
            return Path(found)
    return None
