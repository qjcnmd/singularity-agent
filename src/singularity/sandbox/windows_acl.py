from __future__ import annotations

import shutil
from pathlib import Path

from singularity.sandbox.windows_common import (
    SANDBOX_ACCOUNTS,
    WindowsCapabilityState,
    _cleanup_probe_root,
    _completed_process_diagnostics,
    _exception_diagnostics,
    _hash_path,
    _hash_sid,
    _hash_text,
    _is_relative_to,
    _is_windows,
    _missing,
    _OperationResult,
    _probe_evidence,
    _run_command,
    _runner_result_summary,
    _safe_output,
    _windows_state_dir,
    _windows_state_dir_path,
    _WindowsSandboxIdentity,
)


def _account_python_smoke(*args, **kwargs):
    from singularity.sandbox.windows_runtime import _account_python_smoke as impl

    return impl(*args, **kwargs)


def _account_sid(*args, **kwargs):
    from singularity.sandbox.windows_identity import _account_sid as impl

    return impl(*args, **kwargs)


def _credential_state(*args, **kwargs):
    from singularity.sandbox.windows_doctor import _credential_state as impl

    return impl(*args, **kwargs)


def _current_process_sid(*args, **kwargs):
    from singularity.sandbox.windows_identity import _current_process_sid as impl

    return impl(*args, **kwargs)


def _state_from_bool(*args, **kwargs):
    from singularity.sandbox.windows_doctor import _state_from_bool as impl

    return impl(*args, **kwargs)


def _acl_state(
    platform_supported: bool,
    identity: _WindowsSandboxIdentity,
) -> WindowsCapabilityState:
    if not platform_supported:
        return _missing("Windows ACL boundary requires Windows.", {"tool": "icacls"})
    state_dir = _windows_state_dir_path()
    root = state_dir / "acl-probe"
    sid = _account_sid(identity.account_name)
    if not sid or not _credential_state(identity).ready:
        return _missing(
            "ACL boundary probe requires sandbox account and credential.",
            {
                "probe": "acl_boundary",
                "state_dir_hash": _hash_path(state_dir),
                "probe_root_hash": _hash_path(root),
            },
        )
    try:
        state_dir = _windows_state_dir()
        root = state_dir / "acl-probe"
        root.mkdir(parents=True, exist_ok=True)
        allowed = root / "allowed"
        denied = root / "denied"
        allowed.mkdir(parents=True, exist_ok=True)
        denied.mkdir(parents=True, exist_ok=True)
        control = _apply_probe_root_acl(
            root,
            account_names=(identity.account_name,),
            operation="acl_probe_control_acl",
            low_integrity_root=allowed,
        )
        if not control.ok:
            return _missing(
                "ACL probe control directory setup failed.",
                {
                    **_probe_evidence("acl_probe_control_acl", state_dir=state_dir, probe_root=root, path=root),
                    "reason": control.reason,
                    "details": control.details,
                },
            )
        grant = _apply_probe_root_acl(
            allowed,
            account_names=(identity.account_name,),
            operation="acl_probe_allowed_acl",
        )
        icacls = shutil.which("icacls")
        if icacls is None:
            return _missing(
                "icacls is required for ACL probe.",
                _probe_evidence(
                    "acl_probe_icacls_missing",
                    state_dir=state_dir,
                    probe_root=root,
                    path=denied,
                    extra={"tool": "icacls"},
                ),
            )
        deny = _run_command(
            [
                icacls,
                str(denied),
                "/inheritance:r",
                "/remove:g",
                identity.account_name,
                "/T",
                "/C",
            ],
        )
        if not grant.ok or deny.returncode != 0:
            details = grant.details if not grant.ok else _completed_process_diagnostics(
                "acl_probe_deny_icacls",
                deny,
                state_dir=state_dir,
                probe_root=root,
                path=denied,
            )
            return _missing(
                "ACL probe setup failed.",
                {
                    **_probe_evidence("acl_probe_setup", state_dir=state_dir, probe_root=root),
                    "grant_ok": grant.ok,
                    "deny_exit": deny.returncode,
                    "details": details,
                },
            )
        allowed_result = _account_python_smoke(
            identity=identity,
            cwd=allowed,
            code="from pathlib import Path; Path('ok.txt').write_text('ok', encoding='utf-8')",
            timeout_seconds=5,
            operation_prefix="acl_allowed",
        )
        denied_result = _account_python_smoke(
            identity=identity,
            cwd=root,
            code=(
                "from pathlib import Path\n"
                f"target = Path({str(denied / 'blocked.txt')!r})\n"
                "try:\n"
                "    target.write_text('bad', encoding='utf-8')\n"
                "except OSError:\n"
                "    raise SystemExit(0)\n"
                "raise SystemExit(7)\n"
            ),
            timeout_seconds=5,
            operation_prefix="acl_denied",
        )
        ready = allowed_result.exit_code == 0 and denied_result.exit_code == 0
        evidence = _probe_evidence(
            "acl_boundary",
            state_dir=state_dir,
            probe_root=root,
            extra={
                "account_sid_hash": _hash_sid(sid),
                "allowed": _runner_result_summary(
                    "acl_allowed_write",
                    allowed_result,
                    state_dir=state_dir,
                    probe_root=root,
                    path=allowed,
                ),
                "denied": _runner_result_summary(
                    "acl_denied_write",
                    denied_result,
                    state_dir=state_dir,
                    probe_root=root,
                    path=denied,
                ),
            },
        )
        return _state_from_bool(
            ready,
            "ACL boundary self-test passed for sandbox account.",
            "ACL boundary self-test failed for sandbox account.",
            evidence,
        )
    except OSError as exc:
        return _missing(
            "ACL probe directory could not be created.",
            _exception_diagnostics(
                "acl_probe_root_mkdir",
                exc,
                state_dir=state_dir,
                probe_root=root,
                path=root,
            ),
        )
    finally:
        _cleanup_probe_root(root)

def _ensure_state_dir_acl() -> _OperationResult:
    if not _is_windows():
        return _OperationResult(False, "State directory ACL hardening requires Windows.")
    try:
        state_dir = _windows_state_dir()
    except OSError as exc:
        return _OperationResult(
            False,
            "Windows sandbox state directory could not be created.",
            _exception_diagnostics("state_dir_acl_mkdir", exc, state_dir=_windows_state_dir_path()),
        )
    acl = _apply_sandbox_control_dir_acl(
        state_dir,
        operation="state_dir_acl",
    )
    if not acl.ok:
        return acl
    return _OperationResult(True, "state_dir_acl_applied", {"changed": True, "state_dir_hash": _hash_path(state_dir)})

def _apply_account_acl(
    path: Path,
    *,
    account_names: tuple[str, ...] = SANDBOX_ACCOUNTS,
    low_integrity_root: Path | None = None,
) -> _OperationResult:
    if not _is_windows():
        return _OperationResult(False, "Windows ACL setup requires Windows.")
    icacls = shutil.which("icacls")
    if icacls is None:
        return _OperationResult(False, "icacls is required for sandbox ACL setup.")
    low_integrity_target = low_integrity_root or path
    grant_args = [icacls, str(path), "/grant"]
    grant_args.extend(f"{account}:(OI)(CI)M" for account in account_names)
    grant_args.extend(("/T", "/C"))
    grant = _run_command(grant_args)
    if grant.returncode != 0:
        return _OperationResult(
            False,
            _safe_output(grant),
            _completed_process_diagnostics(
                "acl_grant",
                grant,
                state_dir=_windows_state_dir_path(),
                probe_root=path,
                path=path,
            ),
        )
    integrity = _run_command(
        [
            icacls,
            str(low_integrity_target),
            "/setintegritylevel",
            "(OI)(CI)L",
            "/T",
            "/C",
        ]
    )
    if integrity.returncode != 0:
        return _OperationResult(
            False,
            _safe_output(integrity),
            _completed_process_diagnostics(
                "acl_low_integrity",
                integrity,
                state_dir=_windows_state_dir_path(),
                probe_root=path,
                path=low_integrity_target,
            ),
        )
    return _OperationResult(True)

def _apply_sandbox_control_dir_acl(
    path: Path,
    *,
    account_names: tuple[str, ...] = SANDBOX_ACCOUNTS,
    operation: str = "sandbox_control_dir_acl",
    low_integrity_root: Path | None = None,
) -> _OperationResult:
    if not _is_windows():
        return _OperationResult(False, "Windows sandbox control directory ACL setup requires Windows.")
    state_dir = _windows_state_dir_path().resolve(strict=False)
    target = path.resolve(strict=False)
    if not (_is_relative_to(target, state_dir) or target == state_dir):
        return _OperationResult(
            False,
            "Refusing to grant sandbox account access outside the Windows sandbox state directory.",
            _probe_evidence(
                f"{operation}_unsafe_target",
                state_dir=state_dir,
                probe_root=path,
                path=path,
                extra={"target": "sandbox_control_dir"},
            ),
        )
    host_sid = _current_process_sid()
    icacls = shutil.which("icacls")
    if not host_sid or icacls is None:
        return _OperationResult(
            False,
            "icacls and the current process SID are required for sandbox control directory ACL setup.",
            _probe_evidence(
                f"{operation}_prerequisites",
                state_dir=state_dir,
                probe_root=path,
                path=path,
                extra={
                    "target": "sandbox_control_dir",
                    "host_sid_available": bool(host_sid),
                    "icacls_available": bool(icacls),
                },
            ),
        )
    commands: list[tuple[str, list[str], Path]] = [
        (
            f"{operation}_protect",
            [icacls, str(path), "/inheritance:r", "/T", "/C", "/Q"],
            path,
        ),
        (
            f"{operation}_grant",
            [
                icacls,
                str(path),
                "/grant:r",
                f"*{host_sid}:(OI)(CI)F",
                *(f"{account}:(OI)(CI)M" for account in account_names),
                "/T",
                "/C",
                "/Q",
            ],
            path,
        ),
    ]
    if low_integrity_root is not None:
        commands.append(
            (
                f"{operation}_low_integrity",
                [
                    icacls,
                    str(low_integrity_root),
                    "/setintegritylevel",
                    "(OI)(CI)L",
                    "/T",
                    "/C",
                    "/Q",
                ],
                low_integrity_root,
            )
        )
    details = {
        "target": "sandbox_control_dir",
        "account_name_hashes": [_hash_text(account) for account in account_names],
    }
    for command_operation, command, command_path in commands:
        result = _run_command(command)
        if result.returncode != 0:
            return _OperationResult(
                False,
                _safe_output(result),
                _completed_process_diagnostics(
                    command_operation,
                    result,
                    state_dir=state_dir,
                    probe_root=path,
                    path=command_path,
                    extra=details,
                ),
            )
    return _OperationResult(
        True,
        f"{operation}_ready",
        {
            **_probe_evidence(
                operation,
                state_dir=state_dir,
                probe_root=path,
                path=path,
                extra=details,
            ),
            "changed": True,
        },
    )

def _apply_probe_root_acl(
    path: Path,
    *,
    account_names: tuple[str, ...] = SANDBOX_ACCOUNTS,
    operation: str = "probe_root_acl",
    low_integrity_root: Path | None = None,
) -> _OperationResult:
    return _apply_sandbox_control_dir_acl(
        path,
        account_names=account_names,
        operation=operation,
        low_integrity_root=low_integrity_root,
    )
