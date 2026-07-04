from __future__ import annotations

import re
import shutil
import subprocess
from pathlib import Path
from typing import Any

from singularity.sandbox.windows_common import (
    LEGACY_FIREWALL_RULE_NAME,
    LEGACY_SANDBOX_ACCOUNTS,
    SANDBOX_ACCOUNTS,
    WindowsSandboxCleanupReport,
    _completed_process_diagnostics,
    _exception_diagnostics,
    _hash_path,
    _hash_sid,
    _hash_text,
    _is_elevated,
    _is_relative_to,
    _is_windows,
    _OperationResult,
    _run_command,
    _windows_state_dir_path,
)


def _account_exists(*args, **kwargs):
    from singularity.sandbox.windows_identity import _account_exists as impl

    return impl(*args, **kwargs)


def _account_name_diagnostics(*args, **kwargs):
    from singularity.sandbox.windows_identity import _account_name_diagnostics as impl

    return impl(*args, **kwargs)


def _account_sid(*args, **kwargs):
    from singularity.sandbox.windows_identity import _account_sid as impl

    return impl(*args, **kwargs)


def _credential_exists(*args, **kwargs):
    from singularity.sandbox.windows_identity import _credential_exists as impl

    return impl(*args, **kwargs)


def _current_process_sid(*args, **kwargs):
    from singularity.sandbox.windows_identity import _current_process_sid as impl

    return impl(*args, **kwargs)


def _delete_credential(*args, **kwargs):
    from singularity.sandbox.windows_identity import _delete_credential as impl

    return impl(*args, **kwargs)


def _delete_firewall_rule(*args, **kwargs):
    from singularity.sandbox.windows_firewall import _delete_firewall_rule as impl

    return impl(*args, **kwargs)


def _delete_sandbox_account(*args, **kwargs):
    from singularity.sandbox.windows_identity import _delete_sandbox_account as impl

    return impl(*args, **kwargs)


def _firewall_group_rule_count(*args, **kwargs):
    from singularity.sandbox.windows_firewall import _firewall_group_rule_count as impl

    return impl(*args, **kwargs)


def _legacy_artifact_diagnostics(*args, **kwargs):
    from singularity.sandbox.windows_doctor import _legacy_artifact_diagnostics as impl

    return impl(*args, **kwargs)


def _login_ui_entry_exists(*args, **kwargs):
    from singularity.sandbox.windows_doctor import _login_ui_entry_exists as impl

    return impl(*args, **kwargs)


def _remove_all_account_rights(*args, **kwargs):
    from singularity.sandbox.windows_identity import _remove_all_account_rights as impl

    return impl(*args, **kwargs)


def _remove_login_ui_visibility_entry(*args, **kwargs):
    from singularity.sandbox.windows_identity import _remove_login_ui_visibility_entry as impl

    return impl(*args, **kwargs)


def _remove_runner_runtime_access(*args, **kwargs):
    from singularity.sandbox.windows_runtime import _remove_runner_runtime_access as impl

    return impl(*args, **kwargs)


def _security_attestation_exists(*args, **kwargs):
    from singularity.sandbox.windows_doctor import _security_attestation_exists as impl

    return impl(*args, **kwargs)




def cleanup_windows_sandbox_assets():
    if not _is_windows():
        return WindowsSandboxCleanupReport(
            status="not_supported",
            requested_operation="cleanup",
            requires_elevation=False,
            changed=False,
            completed_steps=(),
            failed_steps=({"step": "platform", "reason": "Windows sandbox cleanup requires Windows."},),
            diagnostics=(),
        )
    if not _is_elevated():
        return WindowsSandboxCleanupReport(
            status="requires_elevation",
            requested_operation="cleanup",
            requires_elevation=True,
            changed=False,
            completed_steps=(),
            failed_steps=(),
            diagnostics=(
                {
                    "kind": "cleanup_requires_elevation",
                    "status": "blocked",
                    "reason": "Run sandbox cleanup from an elevated shell.",
                },
            ),
        )

    changed = False
    completed: list[str] = []
    failed: list[dict[str, object]] = []
    diagnostics: list[dict[str, object]] = list(_legacy_artifact_diagnostics())

    asset_accounts = tuple(dict.fromkeys((*SANDBOX_ACCOUNTS, *LEGACY_SANDBOX_ACCOUNTS)))
    for target in asset_accounts:
        if not target:
            continue
        credential = _delete_credential(target)
        if credential.ok:
            changed = changed or bool(credential.details.get("changed"))
            completed.append(f"credential:{_hash_text(target)}")
        else:
            failed.append(
                {"step": "credential", "reason": credential.reason, "details": credential.details}
            )

    from singularity.sandbox.windows_firewall import _delete_firewall_group

    firewall = _delete_firewall_group()
    if firewall.ok:
        changed = changed or bool(firewall.details.get("changed"))
        completed.append("firewall_group")
    else:
        failed.append(
            {"step": "firewall_group", "reason": firewall.reason, "details": firewall.details}
        )

    from singularity.sandbox.windows_doctor import _delete_security_attestation

    attestation = _delete_security_attestation()
    if attestation.ok:
        changed = changed or bool(attestation.details.get("changed"))
        completed.append("security_attestation")
    else:
        failed.append(
            {
                "step": "security_attestation",
                "reason": attestation.reason,
                "details": attestation.details,
            }
        )

    for target in asset_accounts:
        if not target:
            continue
        visibility = _remove_login_ui_visibility_entry(target)
        if visibility.ok:
            changed = changed or bool(visibility.details.get("changed"))
            completed.append(f"login_ui_visibility:{_hash_text(target)}")
        else:
            failed.append(
                {
                    "step": "login_ui_visibility",
                    "reason": visibility.reason,
                    "details": visibility.details,
                }
            )

    runtime_accounts = tuple(target for target in asset_accounts if target and _account_exists(target))
    runtime_access = _remove_runner_runtime_access(runtime_accounts)
    if runtime_access.ok:
        changed = changed or bool(runtime_access.details.get("changed"))
        completed.append("runner_runtime_access")
    else:
        failed.append(
            {
                "step": "runner_runtime_access",
                "reason": runtime_access.reason,
                "details": runtime_access.details,
            }
        )

    state_dir = _delete_windows_state_dir()
    if state_dir.ok:
        changed = changed or bool(state_dir.details.get("changed"))
        completed.append("state_dir")
    else:
        failed.append({"step": "state_dir", "reason": state_dir.reason, "details": state_dir.details})

    for target in reversed(asset_accounts):
        if not target:
            continue
        sid = _account_sid(target)
        rights = _remove_all_account_rights(sid)
        if not rights.ok:
            failed.append(
                {"step": "account_rights", "reason": rights.reason, "details": rights.details}
            )
        account = _delete_sandbox_account(target)
        if account.ok:
            changed = changed or bool(account.details.get("changed"))
            completed.append(f"sandbox_account:{_hash_text(target)}")
        else:
            failed.append(
                {"step": "sandbox_account", "reason": account.reason, "details": account.details}
            )

    residual_audit = _sandbox_residual_audit()
    residual_count = sum(residual_audit.values())
    if residual_count:
        failed.append(
            {
                "step": "residual_audit",
                "reason": "Singularity Windows sandbox assets remain after cleanup.",
                "details": {"residual_audit": residual_audit},
            }
        )
    else:
        completed.append("residual_audit")
    from singularity.sandbox.windows_doctor import probe_windows_sandbox

    probe_windows_sandbox.cache_clear()
    status = "failed" if failed else "completed"
    if not changed and not failed:
        status = "completed"
    return WindowsSandboxCleanupReport(
        status=status,
        requested_operation="cleanup",
        requires_elevation=False,
        changed=changed,
        completed_steps=tuple(dict.fromkeys(completed)),
        failed_steps=tuple(failed),
        diagnostics=tuple(diagnostics),
        residual_audit=residual_audit,
    )

def _cleanup_legacy_assets() -> _OperationResult:
    changed = False
    failures: list[dict[str, Any]] = []
    for target in LEGACY_SANDBOX_ACCOUNTS:
        for operation, result in (
            ("credential", _delete_credential(target)),
            ("login_ui_visibility", _remove_login_ui_visibility_entry(target)),
        ):
            if result.ok:
                changed = changed or bool(result.details.get("changed"))
            else:
                failures.append(
                    {"operation": operation, "reason": result.reason, "details": result.details}
                )
    legacy_firewall = _delete_firewall_rule(LEGACY_FIREWALL_RULE_NAME)
    if legacy_firewall.ok:
        changed = changed or bool(legacy_firewall.details.get("changed"))
    else:
        failures.append(
            {
                "operation": "firewall_rule",
                "reason": legacy_firewall.reason,
                "details": legacy_firewall.details,
            }
        )
    state_dir = _windows_state_dir_path()
    icacls = shutil.which("icacls")
    if state_dir.exists() and icacls:
        for target in LEGACY_SANDBOX_ACCOUNTS:
            if not _account_exists(target):
                continue
            completed = _run_command(
                [icacls, str(state_dir), "/remove:g", target, "/T", "/C", "/Q"]
            )
            if completed.returncode != 0:
                failures.append(
                    {
                        "operation": "legacy_acl_remove",
                        "details": _completed_process_diagnostics(
                            "legacy_acl_remove",
                            completed,
                            state_dir=state_dir,
                            path=state_dir,
                            extra={"account": _account_name_diagnostics(target)},
                        ),
                    }
                )
            else:
                changed = True
    for target in reversed(LEGACY_SANDBOX_ACCOUNTS):
        if not _account_exists(target):
            continue
        rights = _remove_all_account_rights(_account_sid(target))
        if not rights.ok:
            failures.append(
                {
                    "operation": "legacy_account_rights",
                    "reason": rights.reason,
                    "details": rights.details,
                }
            )
        result = _delete_sandbox_account(target)
        if result.ok:
            changed = changed or bool(result.details.get("changed"))
        else:
            failures.append(
                {"operation": "sandbox_account", "reason": result.reason, "details": result.details}
            )
    if failures:
        return _OperationResult(
            False,
            "Legacy Windows sandbox assets could not be fully removed.",
            {"changed": changed, "failures": failures},
        )
    residuals = _legacy_artifact_diagnostics()
    if residuals:
        return _OperationResult(
            False,
            "Legacy Windows sandbox assets remain after cleanup.",
            {
                "changed": changed,
                "residual_count": len(residuals),
                "residual_kinds": sorted(
                    {str(item.get("kind") or "unknown") for item in residuals}
                ),
            },
        )
    return _OperationResult(True, "legacy_assets_removed", {"changed": changed})

def _sandbox_residual_audit() -> dict[str, int]:
    account_names = (*SANDBOX_ACCOUNTS, *LEGACY_SANDBOX_ACCOUNTS)
    return {
        "accounts": sum(1 for name in account_names if _account_exists(name)),
        "credentials": sum(1 for name in account_names if _credential_exists(name)),
        "firewall_rules": _firewall_group_rule_count(),
        "login_ui_entries": sum(
            1 for name in account_names if _login_ui_entry_exists(name)
        ),
        "security_attestations": int(_security_attestation_exists()),
        "state_dirs": int(_windows_state_dir_path().exists()),
    }

def _delete_windows_state_dir() -> _OperationResult:
    path = _windows_state_dir_path()
    details = {"state_dir_hash": _hash_path(path)}
    if not _is_windows():
        return _OperationResult(False, "Windows state directory cleanup requires Windows.", details)
    normalized = str(path.expanduser().resolve(strict=False)).replace("/", "\\").lower().rstrip("\\")
    if not normalized.endswith("\\singularity\\windows-sandbox"):
        return _OperationResult(
            False,
            "Refusing to delete path outside the Singularity windows-sandbox state directory.",
            details,
        )
    if not path.exists():
        return _OperationResult(True, "state_dir_not_present", {"changed": False, **details})
    tools = {name: shutil.which(name) for name in ("takeown", "icacls", "attrib")}
    missing_tools = sorted(name for name, executable in tools.items() if executable is None)
    if missing_tools:
        return _OperationResult(
            False,
            "Windows state directory cleanup tools are unavailable.",
            {"missing_tools": missing_tools, **details},
        )
    repair_commands = (
        [str(tools["takeown"]), "/F", str(path), "/R", "/D", "Y"],
        [
            str(tools["icacls"]),
            str(path),
            "/inheritance:e",
            "/T",
            "/C",
            "/Q",
        ],
        [
            str(tools["icacls"]),
            str(path),
            "/reset",
            "/T",
            "/C",
            "/Q",
        ],
        [
            str(tools["icacls"]),
            str(path),
            "/setintegritylevel",
            "(OI)(CI)M",
            "/T",
            "/C",
            "/Q",
        ],
        [str(tools["attrib"]), "-R", "-S", "-H", str(path), "/S", "/D"],
        [str(tools["attrib"]), "-R", "-S", "-H", str(path / "*"), "/S", "/D"],
    )
    for command in repair_commands:
        result = _run_command(command)
        if result.returncode != 0:
            return _OperationResult(
                False,
                "Failed to normalize Windows sandbox state directory before deletion.",
                _completed_process_diagnostics(
                    "state_dir_cleanup_normalize",
                    result,
                    state_dir=path,
                    path=path,
                    extra=details,
                ),
            )
    try:
        shutil.rmtree(path)
    except OSError as exc:
        return _OperationResult(
            False,
            "Failed to remove Windows sandbox state directory.",
            _exception_diagnostics("state_dir_cleanup", exc, state_dir=path, path=path),
        )
    return _OperationResult(True, "state_dir_removed", {"changed": True, **details})

def _normalize_run_root_for_cleanup(path: Path) -> _OperationResult:
    state_dir = _windows_state_dir_path().resolve(strict=False)
    runs_dir = (state_dir / "runs").resolve(strict=False)
    candidate = path.resolve(strict=False)
    host_sid = _current_process_sid()
    details = {
        "state_dir_hash": _hash_path(state_dir),
        "run_root_hash": _hash_path(candidate),
        "host_sid_hash": _hash_sid(host_sid),
    }
    if (
        not _is_relative_to(candidate, runs_dir)
        or candidate == runs_dir
        or candidate.parent != runs_dir
        or not candidate.name.startswith("sandbox_")
    ):
        return _OperationResult(
            False,
            "Refusing to normalize a path outside the Windows sandbox run directory.",
            details,
        )
    if not candidate.exists():
        return _OperationResult(True, "run_root_not_present", {"changed": False, **details})
    if not host_sid:
        return _OperationResult(
            False,
            "Windows sandbox run-root cleanup requires the host process SID.",
            details,
        )
    tools = {name: shutil.which(name) for name in ("takeown", "icacls", "attrib")}
    missing = sorted(name for name, executable in tools.items() if executable is None)
    if missing:
        return _OperationResult(
            False,
            "Windows run-root cleanup tools are unavailable.",
            {"missing_tools": missing, **details},
        )
    commands = (
        [str(tools["takeown"]), "/F", str(candidate), "/R", "/D", "Y"],
        [
            str(tools["icacls"]),
            str(candidate),
            "/inheritance:e",
            "/T",
            "/C",
            "/Q",
        ],
        [
            str(tools["icacls"]),
            str(candidate),
            "/reset",
            "/T",
            "/C",
            "/Q",
        ],
        [
            str(tools["icacls"]),
            str(candidate),
            "/grant:r",
            f"*{host_sid}:(OI)(CI)F",
            "/T",
            "/C",
            "/Q",
        ],
        [
            str(tools["icacls"]),
            str(candidate),
            "/setintegritylevel",
            "(OI)(CI)M",
            "/T",
            "/C",
            "/Q",
        ],
        [str(tools["attrib"]), "-R", "-S", "-H", str(candidate), "/S", "/D"],
        [str(tools["attrib"]), "-R", "-S", "-H", str(candidate / "*"), "/S", "/D"],
    )
    for command in commands:
        result = _run_command(command)
        if _cleanup_command_failed(result):
            return _OperationResult(
                False,
                "Failed to normalize Windows sandbox run root before deletion.",
                _completed_process_diagnostics(
                    "run_root_cleanup_normalize",
                    result,
                    state_dir=state_dir,
                    probe_root=candidate,
                    path=candidate,
                    extra=details,
                ),
            )
    return _OperationResult(True, "run_root_normalized", {"changed": True, **details})

def _workspace_cleanup_command(workspace_copy_root: Path) -> list[str]:
    return [str(workspace_copy_root)]

def _cleanup_command_failed(result: subprocess.CompletedProcess[str]) -> bool:
    if result.returncode != 0:
        return True
    output = f"{result.stdout or ''}\n{result.stderr or ''}".lower()
    if re.search(r"failed processing\s+[1-9]\d*\s+files?", output):
        return True
    return "access is denied" in output
