from __future__ import annotations

import singularity.sandbox.windows_common as _windows


def _apply_sandbox_control_dir_acl(
    path,
    *,
    account_names=None,
    operation: str = "sandbox_control_dir_acl",
    low_integrity_root=None,
):
    account_names = account_names if account_names is not None else _windows.SANDBOX_ACCOUNTS
    if not _windows._is_windows():
        return _windows._OperationResult(
            False,
            "Windows sandbox control directory ACL setup requires Windows.",
        )
    state_dir = _windows._windows_state_dir_path().resolve(strict=False)
    target = path.resolve(strict=False)
    if not (_windows._is_relative_to(target, state_dir) or target == state_dir):
        return _windows._OperationResult(
            False,
            "Refusing to grant sandbox account access outside the Windows sandbox state directory.",
            _windows._probe_evidence(
                f"{operation}_unsafe_target",
                state_dir=state_dir,
                probe_root=path,
                path=path,
                extra={"target": "sandbox_control_dir"},
            ),
        )
    host_sid = _windows._current_process_sid()
    icacls = _windows.shutil.which("icacls")
    if not host_sid or icacls is None:
        return _windows._OperationResult(
            False,
            "icacls and the current process SID are required for sandbox control directory ACL setup.",
            _windows._probe_evidence(
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
    commands = [
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
        "account_name_hashes": [_windows._hash_text(account) for account in account_names],
    }
    for command_operation, command, command_path in commands:
        result = _windows._run_command(command)
        if result.returncode != 0:
            return _windows._OperationResult(
                False,
                _windows._safe_output(result),
                _windows._completed_process_diagnostics(
                    command_operation,
                    result,
                    state_dir=state_dir,
                    probe_root=path,
                    path=command_path,
                    extra=details,
                ),
            )
    return _windows._OperationResult(
        True,
        f"{operation}_ready",
        {
            **_windows._probe_evidence(
                operation,
                state_dir=state_dir,
                probe_root=path,
                path=path,
                extra=details,
            ),
            "changed": True,
        },
    )
