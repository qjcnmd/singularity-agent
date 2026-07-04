from __future__ import annotations

import singularity.sandbox.windows_common as _windows


def cleanup_windows_sandbox_assets():
    if not _windows._is_windows():
        return _windows.WindowsSandboxCleanupReport(
            status="not_supported",
            requested_operation="cleanup",
            requires_elevation=False,
            changed=False,
            completed_steps=(),
            failed_steps=({"step": "platform", "reason": "Windows sandbox cleanup requires Windows."},),
            diagnostics=(),
        )
    if not _windows._is_elevated():
        return _windows.WindowsSandboxCleanupReport(
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
    diagnostics: list[dict[str, object]] = list(_windows._legacy_artifact_diagnostics())

    asset_accounts = tuple(dict.fromkeys((*_windows.SANDBOX_ACCOUNTS, *_windows.LEGACY_SANDBOX_ACCOUNTS)))
    for target in asset_accounts:
        if not target:
            continue
        credential = _windows._delete_credential(target)
        if credential.ok:
            changed = changed or bool(credential.details.get("changed"))
            completed.append(f"credential:{_windows._hash_text(target)}")
        else:
            failed.append(
                {"step": "credential", "reason": credential.reason, "details": credential.details}
            )

    firewall = _windows._delete_firewall_group()
    if firewall.ok:
        changed = changed or bool(firewall.details.get("changed"))
        completed.append("firewall_group")
    else:
        failed.append(
            {"step": "firewall_group", "reason": firewall.reason, "details": firewall.details}
        )

    attestation = _windows._delete_security_attestation()
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
        visibility = _windows._remove_login_ui_visibility_entry(target)
        if visibility.ok:
            changed = changed or bool(visibility.details.get("changed"))
            completed.append(f"login_ui_visibility:{_windows._hash_text(target)}")
        else:
            failed.append(
                {
                    "step": "login_ui_visibility",
                    "reason": visibility.reason,
                    "details": visibility.details,
                }
            )

    runtime_accounts = tuple(target for target in asset_accounts if target and _windows._account_exists(target))
    runtime_access = _windows._remove_runner_runtime_access(runtime_accounts)
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

    state_dir = _windows._delete_windows_state_dir()
    if state_dir.ok:
        changed = changed or bool(state_dir.details.get("changed"))
        completed.append("state_dir")
    else:
        failed.append({"step": "state_dir", "reason": state_dir.reason, "details": state_dir.details})

    for target in reversed(asset_accounts):
        if not target:
            continue
        sid = _windows._account_sid(target)
        rights = _windows._remove_all_account_rights(sid)
        if not rights.ok:
            failed.append(
                {"step": "account_rights", "reason": rights.reason, "details": rights.details}
            )
        account = _windows._delete_sandbox_account(target)
        if account.ok:
            changed = changed or bool(account.details.get("changed"))
            completed.append(f"sandbox_account:{_windows._hash_text(target)}")
        else:
            failed.append(
                {"step": "sandbox_account", "reason": account.reason, "details": account.details}
            )

    residual_audit = _windows._sandbox_residual_audit()
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
    _windows.probe_windows_sandbox.cache_clear()
    status = "failed" if failed else "completed"
    if not changed and not failed:
        status = "completed"
    return _windows.WindowsSandboxCleanupReport(
        status=status,
        requested_operation="cleanup",
        requires_elevation=False,
        changed=changed,
        completed_steps=tuple(dict.fromkeys(completed)),
        failed_steps=tuple(failed),
        diagnostics=tuple(diagnostics),
        residual_audit=residual_audit,
    )
