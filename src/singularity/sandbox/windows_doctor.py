from __future__ import annotations

import json
import shutil
import sys
from functools import lru_cache
from pathlib import Path
from typing import Any

from singularity.sandbox.models import PreparedSandbox, SandboxNetworkMode
from singularity.sandbox.windows_common import (
    _SANDBOX_IDENTITIES,
    FIREWALL_RULE_GROUP,
    FIREWALL_RULE_NAME,
    LEGACY_FIREWALL_RULE_NAME,
    LEGACY_SANDBOX_ACCOUNTS,
    LOGIN_UI_USERLIST_KEY,
    SANDBOX_ACCOUNTS,
    SANDBOX_DENY_LOGON_RIGHTS,
    SANDBOX_UNNEEDED_ALLOW_LOGON_RIGHTS,
    SE_DENY_INTERACTIVE_LOGON_NAME,
    SE_INTERACTIVE_LOGON_NAME,
    SECURITY_ATTESTATION_KEY,
    SECURITY_ATTESTATION_POLICY,
    SECURITY_ATTESTATION_SCHEMA_VERSION,
    SECURITY_ATTESTATION_VALUE,
    SETUP_STEP_ORDER,
    WindowsCapabilityState,
    WindowsSandboxDoctorReport,
    WindowsSandboxExecution,
    WindowsSandboxPrimitives,
    WindowsSandboxSetup,
    WindowsSandboxSetupReport,
    _available,
    _completed_process_diagnostics,
    _diagnostic_text,
    _has_windows_symbols,
    _hash_path,
    _hash_sid,
    _hash_text,
    _is_elevated,
    _is_windows,
    _missing,
    _OperationResult,
    _probe_evidence,
    _run_command,
    _run_powershell,
    _safe_output,
    _state_dir_state,
    _windows_state_dir_path,
    _WindowsSandboxIdentity,
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


def _acl_state(*args, **kwargs):
    from singularity.sandbox.windows_acl import _acl_state as impl

    return impl(*args, **kwargs)


def _cleanup_legacy_assets(*args, **kwargs):
    from singularity.sandbox.windows_cleanup import _cleanup_legacy_assets as impl

    return impl(*args, **kwargs)


def _credential_exists(*args, **kwargs):
    from singularity.sandbox.windows_identity import _credential_exists as impl

    return impl(*args, **kwargs)


def _ensure_runner_runtime_access(*args, **kwargs):
    from singularity.sandbox.windows_runtime import _ensure_runner_runtime_access as impl

    return impl(*args, **kwargs)


def _ensure_sandbox_identity(*args, **kwargs):
    from singularity.sandbox.windows_identity import _ensure_sandbox_identity as impl

    return impl(*args, **kwargs)


def _ensure_state_dir_acl(*args, **kwargs):
    from singularity.sandbox.windows_acl import _ensure_state_dir_acl as impl

    return impl(*args, **kwargs)


def _enumerate_account_logon_rights(*args, **kwargs):
    from singularity.sandbox.windows_identity import _enumerate_account_logon_rights as impl

    return impl(*args, **kwargs)


def _firewall_local_user_sddl(*args, **kwargs):
    from singularity.sandbox.windows_firewall import _firewall_local_user_sddl as impl

    return impl(*args, **kwargs)


def _firewall_rule_exists(*args, **kwargs):
    from singularity.sandbox.windows_firewall import _firewall_rule_exists as impl

    return impl(*args, **kwargs)


def _group_membership_probe_command(*args, **kwargs):
    from singularity.sandbox.windows_identity import _group_membership_probe_command as impl

    return impl(*args, **kwargs)


def _logon_rights_view(*args, **kwargs):
    from singularity.sandbox.windows_identity import _logon_rights_view as impl

    return impl(*args, **kwargs)


def _network_probe_state(*args, **kwargs):
    from singularity.sandbox.windows_firewall import _network_probe_state as impl

    return impl(*args, **kwargs)


def _network_state(*args, **kwargs):
    from singularity.sandbox.windows_firewall import _network_state as impl

    return impl(*args, **kwargs)


def _online_network_filter_state(*args, **kwargs):
    from singularity.sandbox.windows_firewall import _online_network_filter_state as impl

    return impl(*args, **kwargs)


def _ps_quote(*args, **kwargs):
    from singularity.sandbox.windows_firewall import _ps_quote as impl

    return impl(*args, **kwargs)


def _python_runtime_smoke_diagnostics(*args, **kwargs):
    from singularity.sandbox.windows_runtime import _python_runtime_smoke_diagnostics as impl

    return impl(*args, **kwargs)


def _redact_account_name(*args, **kwargs):
    from singularity.sandbox.windows_identity import _redact_account_name as impl

    return impl(*args, **kwargs)


def _runner_smoke_state(*args, **kwargs):
    from singularity.sandbox.windows_runtime import _runner_smoke_state as impl

    return impl(*args, **kwargs)


def _setup_identity_security(*args, **kwargs):
    from singularity.sandbox.windows_identity import _setup_identity_security as impl

    return impl(*args, **kwargs)


def _stabilize_login_ui_visibility(*args, **kwargs):
    from singularity.sandbox.windows_identity import _stabilize_login_ui_visibility as impl

    return impl(*args, **kwargs)



@lru_cache(maxsize=1)
def probe_windows_sandbox():
    return _probe_windows_sandbox_uncached()

def _can_ignore_unrelated_network_probe_blocker(
    prepared: PreparedSandbox,
    enforcement: WindowsSandboxDoctorReport,
) -> bool:
    if tuple(enforcement.blocking_requirements) != ("execution:network_probe",):
        return False
    role = str(prepared.baseline.get("sandbox_role") or "")
    if role not in {"offline", "online"}:
        return False
    return _network_probe_state_for_role(enforcement.execution.network_probe, role).ready

def _network_probe_state_for_role(
    state: WindowsCapabilityState,
    role: str,
) -> WindowsCapabilityState:
    principals = state.evidence.get("principals")
    if isinstance(principals, dict):
        payload = principals.get(role)
        if isinstance(payload, dict):
            return WindowsCapabilityState(
                str(payload.get("status") or "missing"),
                bool(payload.get("checked", True)),
                str(payload.get("reason") or ""),
                dict(payload.get("evidence") or {}),
            )
    return state

def setup_windows_sandbox() -> WindowsSandboxSetupReport:
    return _setup_windows_sandbox_v2()

def _setup_windows_sandbox_v2() -> WindowsSandboxSetupReport:
    if not _is_windows():
        return WindowsSandboxSetupReport(
            status="not_supported",
            requested_operation="setup",
            requires_elevation=False,
            changed=False,
            completed_steps=(),
            pending_steps=(),
            failed_steps=(
                {"step": "platform", "reason": "Windows sandbox setup requires Windows."},
            ),
            available_after_setup=False,
            message="Windows sandbox setup is not supported on this platform.",
            diagnostics=(),
        )
    if not _is_elevated():
        return WindowsSandboxSetupReport(
            status="requires_elevation",
            requested_operation="setup",
            requires_elevation=True,
            changed=False,
            completed_steps=(),
            pending_steps=SETUP_STEP_ORDER,
            failed_steps=(),
            available_after_setup=False,
            message="Run sandbox setup from an elevated shell to create account and firewall assets.",
            diagnostics=(),
        )

    changed = False
    completed: list[str] = []
    failed: list[dict[str, Any]] = []
    identities = tuple(_SANDBOX_IDENTITIES.values())

    identity_results: dict[str, _OperationResult] = {}
    for identity in identities:
        result = _ensure_sandbox_identity(identity)
        identity_results[identity.role] = result
        changed = changed or bool(result.details.get("changed"))
        if not result.ok:
            failed.append(
                {
                        "step": str(result.details.get("phase") or "sandbox_accounts"),
                    "reason": result.reason,
                    "details": {
                        "role": identity.role,
                        **result.details,
                    },
                }
            )
    if all(result.ok for result in identity_results.values()):
        completed.extend(("sandbox_accounts", "credentials"))

    security_results: dict[str, _OperationResult] = {}
    for identity in identities:
        if not identity_results.get(identity.role, _OperationResult(False)).ok:
            continue
        result = _setup_identity_security(identity)
        security_results[identity.role] = result
        changed = changed or bool(result.details.get("changed"))
        if not result.ok:
            failed.append(
                {
                    "step": str(result.details.get("phase") or "logon_rights"),
                    "reason": result.reason,
                    "details": {"role": identity.role, **result.details},
                }
            )
    security_ready = len(security_results) == len(identities) and all(
        result.ok for result in security_results.values()
    )
    if security_ready:
        completed.extend(("login_ui_visibility", "group_membership"))
        attestation = _write_security_attestation(
            {
                identity.role: _account_sid(identity.account_name)
                for identity in identities
            }
        )
        changed = changed or bool(attestation.details.get("changed"))
        if attestation.ok:
            completed.append("logon_rights")
        else:
            failed.append(
                {
                    "step": "logon_rights",
                    "reason": attestation.reason,
                    "details": attestation.details,
                }
            )

    state_acl = _ensure_state_dir_acl()
    if state_acl.ok:
        changed = changed or bool(state_acl.details.get("changed"))
        completed.append("state_dir_acl")
    else:
        failed.append(
            {"step": "state_dir_acl", "reason": state_acl.reason, "details": state_acl.details}
        )

    runtime_access = _ensure_runner_runtime_access()
    if runtime_access.ok:
        changed = changed or bool(runtime_access.details.get("changed"))
    else:
        failed.append(
            {
                "step": "execution_backends",
                "reason": runtime_access.reason,
                "details": runtime_access.details,
            }
        )

    offline = _SANDBOX_IDENTITIES[SandboxNetworkMode.DENIED]
    offline_sid = _account_sid(offline.account_name)
    if offline_sid and not _network_state(offline_sid).ready:
        _run_powershell(
            f"Remove-NetFirewallRule -Group {_ps_quote(FIREWALL_RULE_GROUP)} "
            "-ErrorAction SilentlyContinue"
        )
        firewall = _run_powershell(
            "New-NetFirewallRule "
            f"-DisplayName {_ps_quote(FIREWALL_RULE_NAME)} "
            f"-Group {_ps_quote(FIREWALL_RULE_GROUP)} "
            "-Direction Outbound -Action Block -Enabled True "
            f"-LocalUser {_ps_quote(_firewall_local_user_sddl(offline_sid))} | Out-Null"
        )
        if firewall.returncode == 0:
            changed = True
        else:
            failed.append(
                {
                    "step": "offline_network_filter",
                    "reason": _safe_output(firewall),
                }
            )
    online = _SANDBOX_IDENTITIES[SandboxNetworkMode.ALLOWED]
    online_sid = _account_sid(online.account_name)
    network_filter_ready = bool(
        offline_sid
        and online_sid
        and _network_state(offline_sid).ready
        and _online_network_filter_state(online_sid).ready
    )
    if network_filter_ready:
        completed.append("offline_network_filter")
    elif not any(item["step"] == "offline_network_filter" for item in failed):
        failed.append(
            {
                "step": "offline_network_filter",
                "reason": "Offline firewall rule or online-account exclusion was not verified.",
            }
        )

    acl_states = {
        identity.role: _acl_state(True, identity) for identity in identities
    }
    if all(state.ready for state in acl_states.values()):
        completed.append("acl_boundary")
    else:
        failed.append(
            {
                "step": "acl_boundary",
                "reason": "ACL boundary probe failed for one or more sandbox accounts.",
                "details": {
                    role: state.to_dict() for role, state in acl_states.items() if not state.ready
                },
            }
        )

    if _has_windows_symbols("user32", "CreateDesktopW", "CloseDesktop"):
        completed.append("private_desktop")
    else:
        failed.append({"step": "private_desktop", "reason": "CreateDesktopW is unavailable"})

    runner_states = {
        identity.role: _runner_smoke_state(identity) for identity in identities
    }
    if all(state.ready for state in runner_states.values()):
        completed.append("execution_backends")
    else:
        failed.append(
            {
                "step": "execution_backends",
                "reason": "Restricted runner smoke failed for one or more sandbox accounts.",
                "details": {
                    role: state.to_dict() for role, state in runner_states.items() if not state.ready
                },
            }
        )

    network_states = {
        identity.role: _network_probe_state(identity, _account_sid(identity.account_name))
        for identity in identities
    }
    if all(state.ready for state in network_states.values()):
        completed.append("network_probe")
    else:
        failed.append(
            {
                "step": "network_probe",
                "reason": "Offline denied or online allowed network probe failed.",
                "details": {
                    role: state.to_dict() for role, state in network_states.items() if not state.ready
                },
            }
        )

    if not failed:
        legacy_cleanup = _cleanup_legacy_assets()
        if legacy_cleanup.ok:
            changed = changed or bool(legacy_cleanup.details.get("changed"))
            completed.append("legacy_cleanup")
        else:
            failed.append(
                {
                    "step": "legacy_cleanup",
                    "reason": legacy_cleanup.reason,
                    "details": legacy_cleanup.details,
                }
            )

    if not failed:
        visibility = _stabilize_login_ui_visibility(identities)
        changed = changed or bool(visibility.details.get("changed"))
        if not visibility.ok:
            failed.append(
                {
                    "step": "login_ui_visibility",
                    "reason": visibility.reason,
                    "details": visibility.details,
                }
            )

    probe_windows_sandbox.cache_clear()
    doctor = _probe_windows_sandbox_uncached()
    status = "ready" if doctor.available and not failed else ("partial" if completed else "failed")
    if failed and not completed:
        status = "failed"
    pending = [
        step
        for step in SETUP_STEP_ORDER
        if step not in completed and not any(item.get("step") == step for item in failed)
    ]
    return WindowsSandboxSetupReport(
        status=status,
        requested_operation="setup",
        requires_elevation=False,
        changed=changed,
        completed_steps=tuple(dict.fromkeys(completed)),
        pending_steps=tuple(pending),
        failed_steps=tuple(failed),
        available_after_setup=doctor.available and not failed,
        message=_setup_message(doctor, doctor.diagnostics),
        diagnostics=doctor.diagnostics,
    )

def _probe_windows_sandbox_uncached() -> WindowsSandboxDoctorReport:
    platform_supported = _is_windows()
    platform_status = "supported" if platform_supported else "not_supported"
    primitives = WindowsSandboxPrimitives(
        restricted_token=_primitive("advapi32", "CreateRestrictedToken", "OpenProcessToken"),
        job_object=_primitive(
            "kernel32",
            "CreateJobObjectW",
            "SetInformationJobObject",
            "AssignProcessToJobObject",
            "TerminateJobObject",
        ),
        low_integrity=_primitive("advapi32", "ConvertStringSidToSidW", "SetTokenInformation"),
        acl=_command_state("icacls", "ACL command is available."),
        firewall=_powershell_state("Get-NetFirewallRule"),
        private_desktop=_primitive("user32", "CreateDesktopW", "CloseDesktop"),
    )
    identities = tuple(_SANDBOX_IDENTITIES.values())
    sids = {
        identity.role: _account_sid(identity.account_name) if platform_supported else ""
        for identity in identities
    }
    rights = {
        identity.role: (
            _enumerate_account_logon_rights(sids[identity.role])
            if sids[identity.role]
            else _logon_rights_view([], "no_sid")
        )
        for identity in identities
    }
    security_attestation = (
        _security_attestation_state(sids) if platform_supported else _missing(
            "Security attestation requires Windows.",
            {},
        )
    )
    diagnostics = _legacy_artifact_diagnostics() if platform_supported else ()
    state_dir = _state_dir_state() if platform_supported else None
    if state_dir is not None and not state_dir.ready:
        diagnostics = (*diagnostics, {"kind": "windows_sandbox_state_dir", **state_dir.to_dict()})
    account_states = {
        identity.role: _state_from_bool(
            bool(sids[identity.role]),
            "sandbox account exists",
            "sandbox account is missing",
            {
                "account": _account_name_diagnostics(identity.account_name),
                "sid_hash": _hash_sid(sids[identity.role]) if sids[identity.role] else None,
            },
        )
        for identity in identities
    }
    visibility_states = {
        identity.role: _login_ui_visibility_state(identity.account_name)
        for identity in identities
    }
    logon_states = {
        identity.role: _logon_rights_state(
            rights[identity.role],
            attested=security_attestation.ready,
        )
        for identity in identities
    }
    group_states = {
        identity.role: _group_membership_state(identity.account_name)
        for identity in identities
    }
    credential_states = {
        identity.role: _credential_state(identity) for identity in identities
    }
    acl_states = {
        identity.role: _acl_state(platform_supported, identity)
        for identity in identities
    }
    runner_states = {
        identity.role: _runner_smoke_state(identity) for identity in identities
    }
    launcher_states = {
        identity.role: _launcher_state(
            identity,
            sids[identity.role],
            rights[identity.role],
            acl_states[identity.role].ready,
        )
        for identity in identities
    }
    backend_states = {
        identity.role: _execution_backend_state(
            primitives,
            sids[identity.role],
            credential_states[identity.role],
            runner_states[identity.role],
        )
        for identity in identities
    }
    network_filter_states = {
        "offline": _network_state(sids["offline"]),
        "online": _online_network_filter_state(sids["online"]),
    }
    network_probe_states = {
        identity.role: _network_probe_state(identity, sids[identity.role])
        for identity in identities
    }
    runtime_diagnostics = _python_runtime_smoke_diagnostics(identities)
    diagnostics = (*diagnostics, *runtime_diagnostics)
    setup = WindowsSandboxSetup(
        sandbox_accounts=_aggregate_identity_states(
            "Both sandbox accounts exist.",
            "One or more sandbox accounts are missing.",
            account_states,
        ),
        login_ui_visibility=_aggregate_identity_states(
            "Both sandbox accounts are hidden from the standard sign-in list.",
            "One or more sandbox accounts remain visible in the standard sign-in list.",
            visibility_states,
        ),
        logon_rights=_aggregate_identity_states(
            "Both sandbox accounts have hardened logon rights.",
            "One or more sandbox accounts have incomplete logon-right hardening.",
            logon_states,
        ),
        group_membership=_aggregate_identity_states(
            "Both sandbox accounts have constrained local group membership.",
            "One or more sandbox accounts have invalid local group membership.",
            group_states,
        ),
        state_dir_acl=_state_dir_acl_state(),
        acl_boundary=_aggregate_identity_states(
            "ACL boundary probes passed for both sandbox accounts.",
            "One or more sandbox account ACL boundary probes failed.",
            acl_states,
        ),
        offline_network_filter=_aggregate_identity_states(
            "Offline firewall isolation and online exclusion are configured.",
            "Offline firewall isolation or online exclusion is incomplete.",
            network_filter_states,
        ),
        private_desktop=_state_from_bool(
            primitives.private_desktop.ready,
            "private desktop primitive is available",
            "private desktop primitive is missing",
            {"api": "CreateDesktopW"},
        ),
        execution_backends=_aggregate_identity_states(
            "Account-backed execution is available for both sandbox accounts.",
            "Account-backed execution is incomplete for one or more sandbox accounts.",
            backend_states,
        ),
        legacy_assets=_legacy_assets_state(),
    )
    execution = WindowsSandboxExecution(
        account_sids=_aggregate_identity_states(
            "Both sandbox account SIDs resolved.",
            "One or more sandbox account SIDs are unresolved.",
            account_states,
        ),
        credentials=_aggregate_identity_states(
            "Both sandbox credentials are present.",
            "One or more sandbox credentials are missing.",
            credential_states,
        ),
        launchers=_aggregate_identity_states(
            "Both sandbox launchers satisfy their prerequisites.",
            "One or more sandbox launchers are unavailable.",
            launcher_states,
        ),
        runner_smoke=_aggregate_identity_states(
            "Restricted runner smoke passed for both sandbox accounts.",
            "Restricted runner smoke failed for one or more sandbox accounts.",
            runner_states,
        ),
        network_probe=_aggregate_identity_states(
            "Offline denied and online allowed network probes passed.",
            "One or more sandbox network probes failed.",
            network_probe_states,
        ),
    )
    blocking = _blocking_requirements(platform_supported, primitives, setup, execution)
    available = platform_supported and not blocking
    return WindowsSandboxDoctorReport(
        implementation="elevated",
        platform_supported=platform_supported,
        platform_status=platform_status,
        primitives=primitives,
        setup=setup,
        execution=execution,
        available=available,
        enforcement_status="available"
        if available
        else ("not_supported" if not platform_supported else "backend_unavailable"),
        blocking_requirements=tuple(blocking),
        recommended_action=_doctor_recommended_action(available, diagnostics),
        diagnostics=diagnostics,
    )

def _blocking_requirements(
    platform_supported: bool,
    primitives: WindowsSandboxPrimitives,
    setup: WindowsSandboxSetup,
    execution: WindowsSandboxExecution,
) -> list[str]:
    blocking = [] if platform_supported else ["platform"]
    for group_name, values in (
        ("primitive", primitives.to_dict()),
        ("setup", setup.to_dict()),
        ("execution", execution.to_dict()),
    ):
        for name, payload in values.items():
            if payload.get("status") != "available":
                blocking.append(f"{group_name}:{name}")
    return blocking

def _primitive(library: str, *symbols: str) -> WindowsCapabilityState:
    if not _is_windows():
        return WindowsCapabilityState(
            status="not_supported",
            checked=True,
            reason="Windows primitive probe requires Windows.",
            evidence={"library": library, "symbols": list(symbols)},
        )
    missing = [symbol for symbol in symbols if not _has_windows_symbols(library, symbol)]
    if missing:
        return WindowsCapabilityState(
            status="missing",
            checked=True,
            reason=f"Missing Windows symbols: {', '.join(missing)}.",
            evidence={"library": library, "missing_symbols": missing},
        )
    return WindowsCapabilityState(
        status="available",
        checked=True,
        reason="Windows symbols are available.",
        evidence={"library": library, "symbols": list(symbols)},
    )

def _command_state(command: str, available_reason: str) -> WindowsCapabilityState:
    executable = shutil.which(command)
    return _state_from_bool(
        executable is not None,
        available_reason,
        f"{command} is missing.",
        {"command": command, "path_hash": _hash_text(executable or "") if executable else None},
    )

def _powershell_state(command: str) -> WindowsCapabilityState:
    if not _is_windows():
        return WindowsCapabilityState(
            status="not_supported",
            checked=True,
            reason="PowerShell NetSecurity probe requires Windows.",
            evidence={"command": command},
        )
    completed = _run_powershell(f"if (Get-Command {command} -ErrorAction SilentlyContinue) {{ exit 0 }}; exit 1")
    return _state_from_bool(
        completed.returncode == 0,
        f"{command} is available.",
        f"{command} is unavailable.",
        {"command": command},
    )

def _execution_backend_state(
    primitives: WindowsSandboxPrimitives,
    sid: str,
    credential_state: WindowsCapabilityState,
    runner_state: WindowsCapabilityState,
) -> WindowsCapabilityState:
    ready = (
        primitives.restricted_token.ready
        and primitives.job_object.ready
        and primitives.low_integrity.ready
        and primitives.private_desktop.ready
        and bool(sid)
        and credential_state.ready
        and runner_state.ready
    )
    return _state_from_bool(
        ready,
        "Windows account-backed execution smoke is available.",
        "Windows account-backed execution smoke is incomplete.",
        {"runner": "windows_runner.py", "account_sid_hash": _hash_sid(sid) if sid else None},
    )

def _executable_acl_summary() -> str:
    icacls = shutil.which("icacls")
    if not icacls:
        return ""
    return _safe_output(_run_command([icacls, sys.executable]))

def _launcher_state(
    identity: _WindowsSandboxIdentity,
    sid: str,
    logon_rights: dict[str, Any],
    acl_boundary_ready: bool,
) -> WindowsCapabilityState:
    if not _is_windows():
        return _missing("Windows launcher probe requires Windows.", {"api": "CreateProcessWithLogonW"})
    symbol_present = _has_windows_symbols("advapi32", "CreateProcessWithLogonW") and _has_windows_symbols(
        "advapi32", "CreateProcessAsUserW"
    )
    interactive = bool(logon_rights.get("interactive"))
    deny_interactive = bool(logon_rights.get("deny_interactive"))
    lsa_status = str(logon_rights.get("lsa_status", ""))
    # LsaEnumerateAccountRights definitively proves the right is absent only when
    # it succeeds (lsa_status empty -> the rights list is authoritative) or
    # reports the account has no LSA row (STATUS_OBJECT_NAME_NOT_FOUND
    # 0xC0000034). A non-elevated caller may receive STATUS_ACCESS_DENIED
    # (0xC0000022) for an account that DOES hold rights; in that case we cannot
    # prove absence and defer to the empirical runner_smoke probe rather than
    # falsely blocking the backend after an elevated setup granted the right.
    rights_definitively_missing = (not interactive) and lsa_status in {"", "0xC0000034"}
    evidence = {
        "api": "CreateProcessWithLogonW",
        "logon_flags": "0 (profile not loaded)",
        "domain_username_form": f".\\{_redact_account_name(identity.account_name)}",
        "symbol_present": symbol_present,
        "account_logon_rights": logon_rights,
        "window_station": {
            "lpDesktop": None,
            "inherits_parent": True,
            "access": "inherited_default (account relies on the inherited window-station DACL)",
        },
        "desktop": {
            "inherits_parent": True,
            "access": "inherited_default (account relies on the inherited desktop DACL)",
        },
        "executable": {
            "path_hash": _hash_text(sys.executable),
            "acl_summary_redacted": _diagnostic_text(
                _executable_acl_summary(),
                path=Path(sys.executable),
            ),
        },
        "working_directory": {
            "representative_hash": _hash_path(_windows_state_dir_path()),
            "account_has_access": acl_boundary_ready,
            "failure_target": None if acl_boundary_ready else "working_directory_access",
        },
    }
    ready = (
        symbol_present
        and not rights_definitively_missing
        and not deny_interactive
        and acl_boundary_ready
    )
    missing_reason = (
        "CreateProcessWithLogonW preconditions missing (working directory account access is missing)."
        if not acl_boundary_ready
        else "CreateProcessWithLogonW preconditions missing (account definitively lacks SeInteractiveLogonRight, has a deny right, or symbols missing)."
    )
    return _state_from_bool(
        ready,
        "CreateProcessWithLogonW preconditions satisfied (SeInteractiveLogonRight present or unverifiable non-elevated, no deny right, symbols available).",
        missing_reason,
        evidence,
    )

def _credential_state(identity: _WindowsSandboxIdentity) -> WindowsCapabilityState:
    # We intentionally do not read or print credential material. Presence is
    # tested through the Windows Credential Manager target only.
    evidence = {
        "storage_scope": "windows_credential_manager",
        "target_hash": _hash_text(identity.credential_target),
        "target_redacted": _redact_account_name(identity.credential_target),
    }
    if not _is_windows():
        return _missing("Credential Manager probe requires Windows.", evidence)
    ready = _credential_exists(identity.credential_target)
    return _state_from_bool(
        ready,
        "Sandbox credential target is present.",
        "Sandbox credential target is missing.",
        evidence,
    )

def _runner_state() -> WindowsCapabilityState:
    runner_path = Path(__file__).with_name("windows_runner.py")
    return _state_from_bool(
        runner_path.exists(),
        "Windows runner entrypoint exists.",
        "Windows runner entrypoint is missing.",
        {"runner_hash": _hash_text(str(runner_path))},
    )

def _state_from_bool(
    ready: bool,
    available_reason: str,
    missing_reason: str,
    evidence: dict[str, Any],
) -> WindowsCapabilityState:
    return WindowsCapabilityState(
        status="available" if ready else "missing",
        checked=True,
        reason=available_reason if ready else missing_reason,
        evidence=evidence,
    )

def _aggregate_identity_states(
    available_reason: str,
    missing_reason: str,
    states: dict[str, WindowsCapabilityState],
) -> WindowsCapabilityState:
    ready = bool(states) and all(state.ready for state in states.values())
    return _state_from_bool(
        ready,
        available_reason,
        missing_reason,
        {"principals": {role: state.to_dict() for role, state in states.items()}},
    )

def _legacy_assets_state() -> WindowsCapabilityState:
    diagnostics = _legacy_artifact_diagnostics()
    return _state_from_bool(
        not diagnostics,
        "Legacy Windows sandbox assets are absent.",
        "Legacy Windows sandbox assets remain and must be removed.",
        {
            "residual_count": len(diagnostics),
            "residual_kinds": sorted(
                {str(item.get("kind") or "unknown") for item in diagnostics}
            ),
        },
    )

def _login_ui_visibility_state(account_name: str) -> WindowsCapabilityState:
    evidence = {
        "registry_key_hash": _hash_text(LOGIN_UI_USERLIST_KEY),
        "account": _account_name_diagnostics(account_name),
        "codex_like_principle": "dedicated sandbox account should not pollute normal sign-in UI",
    }
    if not _is_windows():
        return _missing("Login UI visibility probe requires Windows.", evidence)
    result = _run_powershell(
        "$key = "
        f"{_ps_quote(LOGIN_UI_USERLIST_KEY)}; "
        f"$name = {_ps_quote(account_name)}; "
        "$value = Get-ItemPropertyValue -LiteralPath $key -Name $name "
        "-ErrorAction SilentlyContinue; if ($null -eq $value) { exit 2 }; "
        "if ([int]$value -eq 0) { exit 0 }; exit 1"
    )
    if result.returncode == 0:
        return _available("Sandbox account is hidden from the standard Windows sign-in user list.", evidence)
    details = _completed_process_diagnostics(
        "login_ui_visibility_probe",
        result,
        state_dir=_windows_state_dir_path(),
        extra=evidence,
    )
    reason = (
        "Sandbox account is not hidden from the standard Windows sign-in user list."
        if result.returncode == 1
        else "Sandbox account login UI visibility registry entry is missing."
    )
    return _missing(reason, details)

def _logon_rights_state(
    logon_rights: dict[str, Any],
    *,
    attested: bool = False,
) -> WindowsCapabilityState:
    interactive = bool(logon_rights.get("interactive"))
    deny_interactive = bool(logon_rights.get("deny_interactive"))
    deny_ready = all(
        bool(logon_rights.get(key))
        for key in (
            "deny_remote_interactive",
            "deny_network",
            "deny_service",
            "deny_batch",
        )
    )
    allow_clear = not any(
        bool(logon_rights.get(key))
        for key in ("remote_interactive", "network", "service", "batch")
    )
    directly_verified = interactive and not deny_interactive and deny_ready and allow_clear
    attestation_verified = (
        attested and str(logon_rights.get("lsa_status") or "").upper() == "0XC0000022"
    )
    ready = directly_verified or attestation_verified
    evidence = {
        "logon_rights": logon_rights,
        "verification_source": (
            "direct_lsa_enumeration"
            if directly_verified
            else "protected_setup_attestation"
            if attestation_verified
            else "unverified"
        ),
        "required_allow": [SE_INTERACTIVE_LOGON_NAME],
        "required_absent": [SE_DENY_INTERACTIVE_LOGON_NAME, *SANDBOX_UNNEEDED_ALLOW_LOGON_RIGHTS],
        "required_deny": list(SANDBOX_DENY_LOGON_RIGHTS),
        "interactive_logon_note": (
            "SeInteractiveLogonRight is retained for CreateProcessWithLogonW; "
            "ordinary sign-in exposure is controlled by login UI hiding plus deny rights."
        ),
    }
    return _state_from_bool(
        ready,
        "Sandbox account logon rights are hardened.",
        "Sandbox account logon rights are incomplete or overexposed.",
        evidence,
    )

def _group_membership_state(account_name: str) -> WindowsCapabilityState:
    evidence = {
        "account": _account_name_diagnostics(account_name),
        "required_group_sid_hash": _hash_sid("S-1-5-32-545"),
        "allowed_direct_group_count": 1,
    }
    if not _is_windows():
        return _missing("Users group membership probe requires Windows.", evidence)
    result = _run_powershell(_group_membership_probe_command(account_name))
    return _state_from_bool(
        result.returncode == 0,
        "Sandbox account direct group membership is limited to built-in Users.",
        "Sandbox account has missing or overprivileged direct local group membership.",
        evidence
        if result.returncode == 0
        else _completed_process_diagnostics(
            "group_membership_probe",
            result,
            state_dir=_windows_state_dir_path(),
            extra=evidence,
        ),
    )

def _state_dir_acl_state() -> WindowsCapabilityState:
    state_dir = _windows_state_dir_path()
    evidence = _probe_evidence("state_dir_acl", state_dir=state_dir, path=state_dir)
    if not _is_windows():
        return _missing("State directory ACL probe requires Windows.", evidence)
    if not state_dir.exists():
        return _missing("Windows sandbox state directory is missing.", evidence)
    icacls = shutil.which("icacls")
    if icacls is None:
        return _missing("icacls is required for state directory ACL probe.", evidence)
    result = _run_command([icacls, str(state_dir)])
    text = f"{result.stdout}\n{result.stderr}"
    missing_accounts = [
        account for account in SANDBOX_ACCOUNTS if account.lower() not in text.lower()
    ]
    ready = result.returncode == 0 and not missing_accounts
    details = (
        evidence
        if ready
        else _completed_process_diagnostics(
            "state_dir_acl_probe",
            result,
            state_dir=state_dir,
            path=state_dir,
        )
    )
    return _state_from_bool(
        ready,
        "Windows sandbox state directory ACL includes both sandbox accounts.",
        "Windows sandbox state directory ACL is missing sandbox account access.",
        details,
    )

def _security_attestation_state(sids: dict[str, str]) -> WindowsCapabilityState:
    evidence = {
        "registry_key_hash": _hash_text(SECURITY_ATTESTATION_KEY),
        "principal_count": len(_SANDBOX_IDENTITIES),
        "schema_version": SECURITY_ATTESTATION_SCHEMA_VERSION,
    }
    if not _is_windows():
        return _missing("Security attestation requires Windows.", evidence)
    result = _run_powershell(
        "$subkey = 'SOFTWARE\\Singularity\\WindowsSandbox'; "
        f"$name = {_ps_quote(SECURITY_ATTESTATION_VALUE)}; "
        "$read = [System.Security.AccessControl.RegistryRights]::ReadKey -bor "
        "[System.Security.AccessControl.RegistryRights]::ReadPermissions; "
        "$key = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey("
        "$subkey, [Microsoft.Win32.RegistryKeyPermissionCheck]::ReadSubTree, $read); "
        "if ($null -eq $key) { exit 2 }; $value = $key.GetValue($name, $null); "
        "if ($null -eq $value) { $key.Close(); exit 2 }; "
        "$acl = $key.GetAccessControl(); $key.Close(); "
        "$allowed = @('S-1-5-18', 'S-1-5-32-544'); "
        "$writeMask = [int]("
        "[System.Security.AccessControl.RegistryRights]::SetValue -bor "
        "[System.Security.AccessControl.RegistryRights]::CreateSubKey -bor "
        "[System.Security.AccessControl.RegistryRights]::Delete -bor "
        "[System.Security.AccessControl.RegistryRights]::ChangePermissions -bor "
        "[System.Security.AccessControl.RegistryRights]::TakeOwnership); "
        "$rules = @($acl.GetAccessRules($true, $true, "
        "[System.Security.Principal.SecurityIdentifier])); "
        "$unsafe = @($rules | Where-Object { "
        "$_.AccessControlType -eq 'Allow' -and "
        "(([int]$_.RegistryRights -band $writeMask) -ne 0) -and "
        "$allowed -notcontains $_.IdentityReference.Value }); "
        "if (-not $acl.AreAccessRulesProtected -or $unsafe.Count -ne 0) { exit 3 }; "
        "$payload = $value | ConvertFrom-Json; "
        "$payload | Add-Member -NotePropertyName acl_protected -NotePropertyValue $true -Force; "
        "$payload | ConvertTo-Json -Compress -Depth 6"
    )
    if result.returncode != 0:
        return _missing(
            "Protected sandbox security attestation is missing or has an unsafe ACL.",
            _completed_process_diagnostics(
                "security_attestation_probe",
                result,
                state_dir=_windows_state_dir_path(),
                extra=evidence,
            ),
        )
    try:
        payload = json.loads(result.stdout or "{}")
    except (TypeError, json.JSONDecodeError):
        return _missing("Sandbox security attestation is malformed.", evidence)
    expected = {
        role: _hash_sid(sids.get(role, ""))
        for role in ("offline", "online")
        if sids.get(role)
    }
    principals = payload.get("principals")
    ready = (
        payload.get("schema_version") == SECURITY_ATTESTATION_SCHEMA_VERSION
        and payload.get("policy") == SECURITY_ATTESTATION_POLICY
        and payload.get("acl_protected") is True
        and expected.keys() == {"offline", "online"}
        and principals == expected
    )
    return _state_from_bool(
        ready,
        "Protected setup attestation matches both current sandbox principals.",
        "Protected setup attestation does not match the current sandbox principals.",
        evidence,
    )

def _write_security_attestation(sids: dict[str, str]) -> _OperationResult:
    expected = {
        role: _hash_sid(sids.get(role, ""))
        for role in ("offline", "online")
        if sids.get(role)
    }
    if expected.keys() != {"offline", "online"}:
        return _OperationResult(False, "Both sandbox SIDs are required for security attestation.")
    before = _security_attestation_state(sids)
    if before.ready:
        return _OperationResult(True, "already_attested", {"changed": False})
    payload = json.dumps(
        {
            "schema_version": SECURITY_ATTESTATION_SCHEMA_VERSION,
            "policy": SECURITY_ATTESTATION_POLICY,
            "principals": expected,
        },
        separators=(",", ":"),
        sort_keys=True,
    )
    result = _run_powershell(
        "$subkey = 'SOFTWARE\\Singularity\\WindowsSandbox'; "
        f"$name = {_ps_quote(SECURITY_ATTESTATION_VALUE)}; "
        "$key = [Microsoft.Win32.Registry]::LocalMachine.CreateSubKey("
        "$subkey, [Microsoft.Win32.RegistryKeyPermissionCheck]::ReadWriteSubTree); "
        "if ($null -eq $key) { exit 2 }; "
        f"$key.SetValue($name, {_ps_quote(payload)}, [Microsoft.Win32.RegistryValueKind]::String); "
        "$acl = New-Object System.Security.AccessControl.RegistrySecurity; "
        "$system = New-Object System.Security.Principal.SecurityIdentifier('S-1-5-18'); "
        "$admins = New-Object System.Security.Principal.SecurityIdentifier('S-1-5-32-544'); "
        "$users = New-Object System.Security.Principal.SecurityIdentifier('S-1-5-32-545'); "
        "$allow = [System.Security.AccessControl.AccessControlType]::Allow; "
        "$noneI = [System.Security.AccessControl.InheritanceFlags]::None; "
        "$noneP = [System.Security.AccessControl.PropagationFlags]::None; "
        "$acl.SetOwner($admins); $acl.SetAccessRuleProtection($true, $false); "
        "$acl.AddAccessRule([System.Security.AccessControl.RegistryAccessRule]::new("
        "$system, [System.Security.AccessControl.RegistryRights]::FullControl, "
        "$noneI, $noneP, $allow)); "
        "$acl.AddAccessRule([System.Security.AccessControl.RegistryAccessRule]::new("
        "$admins, [System.Security.AccessControl.RegistryRights]::FullControl, "
        "$noneI, $noneP, $allow)); "
        "$acl.AddAccessRule([System.Security.AccessControl.RegistryAccessRule]::new("
        "$users, [System.Security.AccessControl.RegistryRights]::ReadKey, "
        "$noneI, $noneP, $allow)); $key.SetAccessControl($acl); $key.Close()"
    )
    if result.returncode != 0:
        return _OperationResult(
            False,
            "Failed to write protected sandbox security attestation.",
            _completed_process_diagnostics(
                "security_attestation_write",
                result,
                state_dir=_windows_state_dir_path(),
            ),
        )
    after = _security_attestation_state(sids)
    return _OperationResult(
        after.ready,
        "security_attestation_verified" if after.ready else after.reason,
        {"changed": True, "state": after.to_dict()},
    )

def _security_attestation_exists() -> bool:
    if not _is_windows():
        return False
    result = _run_powershell(
        f"if (Test-Path -LiteralPath {_ps_quote(SECURITY_ATTESTATION_KEY)}) "
        "{ exit 0 } else { exit 2 }"
    )
    return result.returncode == 0

def _delete_security_attestation() -> _OperationResult:
    if not _is_windows():
        return _OperationResult(False, "Security attestation cleanup requires Windows.")
    existed = _security_attestation_exists()
    if not existed:
        return _OperationResult(True, "security_attestation_not_present", {"changed": False})
    result = _run_powershell(
        "$key = "
        f"{_ps_quote(SECURITY_ATTESTATION_KEY)}; "
        "$parent = 'HKLM:\\SOFTWARE\\Singularity'; "
        "Remove-Item -LiteralPath $key -Recurse -Force -ErrorAction Stop; "
        "if (Test-Path -LiteralPath $parent) { "
        "$item = Get-Item -LiteralPath $parent; "
        "if (@(Get-ChildItem -LiteralPath $parent).Count -eq 0 -and "
        "$item.Property.Count -eq 0) { Remove-Item -LiteralPath $parent -Force } }"
    )
    if result.returncode == 0 and not _security_attestation_exists():
        return _OperationResult(True, "security_attestation_removed", {"changed": True})
    return _OperationResult(
        False,
        "Failed to remove sandbox security attestation.",
        _completed_process_diagnostics(
            "security_attestation_cleanup",
            result,
            state_dir=_windows_state_dir_path(),
        ),
    )

def _doctor_recommended_action(
    available: bool,
    diagnostics: tuple[dict[str, Any], ...],
) -> str:
    if available and not diagnostics:
        return "Windows sandbox is ready."
    action = (
        "Windows sandbox is ready."
        if available
        else "Run `singularity-agent sandbox setup --json` from an elevated shell and rerun doctor."
    )
    if diagnostics:
        return f"{action} {_diagnostic_action_suffix(diagnostics)}"
    return action

def _setup_message(
    doctor: WindowsSandboxDoctorReport,
    diagnostics: tuple[dict[str, Any], ...],
) -> str:
    message = "Windows sandbox setup completed." if doctor.available else doctor.reason
    if diagnostics:
        return f"{message} {_diagnostic_action_suffix(diagnostics)}"
    return message

def _diagnostic_action_suffix(diagnostics: tuple[dict[str, Any], ...]) -> str:
    kinds = {str(item.get("kind") or "") for item in diagnostics}
    if kinds and kinds <= {"python_runtime_environment_blocker"}:
        return "Python runtime diagnostics detected; review diagnostics before capability evaluation."
    if "python_runtime_environment_blocker" in kinds:
        return "Python runtime diagnostics and legacy sandbox artifacts detected; review diagnostics."
    return "Legacy sandbox artifacts detected; review diagnostics before cleanup."

def _legacy_artifact_diagnostics() -> tuple[dict[str, Any], ...]:
    diagnostics: list[dict[str, Any]] = []
    for account_name in LEGACY_SANDBOX_ACCOUNTS:
        if _account_exists(account_name):
            diagnostics.append(
                {
                    "kind": "legacy_sandbox_account",
                    "status": "present",
                    **_account_name_diagnostics(account_name),
                }
            )
        if _credential_exists(account_name):
            diagnostics.append(
                {
                    "kind": "legacy_credential",
                    "status": "present",
                    "target_hash": _hash_text(account_name),
                    "target_redacted": _redact_account_name(account_name),
                }
            )
        if _login_ui_entry_exists(account_name):
            diagnostics.append(
                {
                    "kind": "legacy_login_ui_visibility",
                    "status": "present",
                    **_account_name_diagnostics(account_name),
                }
            )
    if (
        LEGACY_FIREWALL_RULE_NAME
        and LEGACY_FIREWALL_RULE_NAME != FIREWALL_RULE_NAME
        and _firewall_rule_exists(LEGACY_FIREWALL_RULE_NAME)
    ):
        diagnostics.append(
            {
                "kind": "legacy_firewall_rule",
                "status": "present",
                "rule_hash": _hash_text(LEGACY_FIREWALL_RULE_NAME),
                "rule_redacted": _redact_account_name(LEGACY_FIREWALL_RULE_NAME),
                "group": FIREWALL_RULE_GROUP,
            }
        )
    return tuple(diagnostics)

def _login_ui_entry_exists(account_name: str) -> bool:
    if not _is_windows():
        return False
    result = _run_powershell(
        "$key = "
        f"{_ps_quote(LOGIN_UI_USERLIST_KEY)}; "
        f"$name = {_ps_quote(account_name)}; "
        "$item = Get-ItemProperty -Path $key -Name $name -ErrorAction SilentlyContinue; "
        "if ($null -ne $item) { exit 0 }; exit 1"
    )
    return result.returncode == 0
