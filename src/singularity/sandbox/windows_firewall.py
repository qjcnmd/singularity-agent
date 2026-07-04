from __future__ import annotations

import json
import socket
import sys
from types import SimpleNamespace
from typing import Any

from singularity.sandbox.models import SandboxNetworkMode, SandboxResourceLimits
from singularity.sandbox.windows_common import (
    FIREWALL_RULE_GROUP,
    FIREWALL_RULE_NAME,
    WindowsCapabilityState,
    WindowsSandboxRunner,
    _account_runner_launch_exception_diagnostics,
    _cleanup_probe_root,
    _completed_process_diagnostics,
    _exception_diagnostics,
    _hash_sid,
    _hash_text,
    _is_windows,
    _missing,
    _OperationResult,
    _probe_evidence,
    _run_powershell,
    _runner_result_operation,
    _runner_result_summary,
    _windows_state_dir,
    _windows_state_dir_path,
    _WindowsSandboxIdentity,
)
from singularity.sandbox.windows_runner import NETWORK_PROBE_ENDPOINTS, WindowsRunnerSpec


def _apply_probe_root_acl(*args, **kwargs):
    from singularity.sandbox.windows_acl import _apply_probe_root_acl as impl

    return impl(*args, **kwargs)


def _runtime_env(*args, **kwargs):
    from singularity.sandbox.windows_runtime import _runtime_env as impl

    return impl(*args, **kwargs)


def _redact_account_name(*args, **kwargs):
    from singularity.sandbox.windows_identity import _redact_account_name as impl

    return impl(*args, **kwargs)


def _state_from_bool(*args, **kwargs):
    from singularity.sandbox.windows_doctor import _state_from_bool as impl

    return impl(*args, **kwargs)


def _network_state(sid: str) -> WindowsCapabilityState:
    if not _is_windows():
        return _missing("Network filter requires Windows Firewall.", {"group": FIREWALL_RULE_GROUP})
    if not sid:
        return _missing("Network filter requires sandbox account SID.", {"group": FIREWALL_RULE_GROUP})
    sid_literal = _ps_quote(sid)
    rule_name = _ps_quote(FIREWALL_RULE_NAME)
    command = (
        f"$rule = Get-NetFirewallRule -DisplayName {rule_name} -ErrorAction SilentlyContinue; "
        "if (-not $rule) { exit 1 }; "
        "$security = $rule | Get-NetFirewallSecurityFilter -ErrorAction SilentlyContinue; "
        f"if ($rule.Enabled -eq 'True' -and $rule.Direction -eq 'Outbound' -and "
        f"$rule.Action -eq 'Block' -and ($security.LocalUser -like ('*' + {sid_literal} + '*'))) "
        "{ exit 0 }; exit 1"
    )
    completed = _run_powershell(command)
    return _state_from_bool(
        completed.returncode == 0,
        "Outbound firewall rule is configured.",
        "Outbound firewall rule for sandbox account is missing or incomplete.",
        {
            "rule_hash": _hash_text(FIREWALL_RULE_NAME),
            "rule_redacted": _redact_account_name(FIREWALL_RULE_NAME),
            "group": FIREWALL_RULE_GROUP,
            "local_user_sid_hash": _hash_sid(sid),
        },
    )

def _online_network_filter_state(sid: str) -> WindowsCapabilityState:
    evidence = {
        "group": FIREWALL_RULE_GROUP,
        "local_user_sid_hash": _hash_sid(sid) if sid else None,
    }
    if not _is_windows():
        return _missing("Online network filter probe requires Windows.", evidence)
    if not sid:
        return _missing("Online sandbox account SID is unavailable.", evidence)
    completed = _run_powershell(
        f"$sid = {_ps_quote(sid)}; "
        f"$rules = Get-NetFirewallRule -Group {_ps_quote(FIREWALL_RULE_GROUP)} "
        "-ErrorAction SilentlyContinue; "
        "$blocked = $rules | Get-NetFirewallSecurityFilter -ErrorAction SilentlyContinue "
        "| Where-Object { $_.LocalUser -like ('*' + $sid + '*') }; "
        "if ($blocked) { exit 1 }; exit 0"
    )
    return _state_from_bool(
        completed.returncode == 0,
        "Online sandbox account is not targeted by Singularity firewall rules.",
        "Online sandbox account is incorrectly targeted by a Singularity firewall rule.",
        evidence,
    )

def _network_probe_state(
    identity: _WindowsSandboxIdentity,
    sid: str,
) -> WindowsCapabilityState:
    if not _is_windows():
        return _missing("Network probe requires Windows.", {"probe": "socket connect"})
    if not sid or (identity.firewall_blocked and not _network_state(sid).ready):
        state_dir = _windows_state_dir_path()
        return _missing(
            "Network probe requires configured firewall rule.",
            _probe_evidence(
                "network_probe_firewall_rule_missing",
                state_dir=state_dir,
                probe_root=state_dir / "network-smoke",
                extra={"probe": "socket connect", "local_user_sid_hash": _hash_sid(sid) if sid else None},
            ),
        )
    host_baseline = _host_network_baseline_state()
    if not host_baseline.ready:
        return host_baseline
    state_dir = _windows_state_dir_path()
    root = state_dir / "network-smoke"
    try:
        state_dir = _windows_state_dir()
        root = state_dir / "network-smoke"
        root.mkdir(parents=True, exist_ok=True)
        acl = _apply_probe_root_acl(
            root,
            account_names=(identity.account_name,),
            operation="network_probe_acl",
        )
        if not acl.ok:
            return _missing(
                "Network denied smoke ACL setup failed for sandbox account.",
                {
                    **_probe_evidence("network_probe_acl", state_dir=state_dir, probe_root=root),
                    "probe": "runtime",
                    "reason": acl.reason,
                    "details": acl.details,
                },
            )
        spec_path = root / "runner-spec.json"
        result_path = root / "runner-result.json"
        if identity.firewall_blocked:
            command = [sys.executable, "-c", "print('network-smoke')"]
            network_mode = SandboxNetworkMode.DENIED.value
        else:
            endpoints = json.dumps(NETWORK_PROBE_ENDPOINTS)
            command = [
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
                    "    print('network-allowed')\n"
                    "    raise SystemExit(0)\n"
                    "raise SystemExit(7)\n"
                ),
            ]
            network_mode = SandboxNetworkMode.ALLOWED.value
        spec = WindowsRunnerSpec(
            command=command,
            cwd=str(root),
            env=_runtime_env({}),
            timeout_seconds=5,
            max_output_chars=2000,
            network_mode=network_mode,
            result_path=str(result_path),
        )
        try:
            spec_path.write_text(json.dumps(spec.to_dict(), ensure_ascii=False), encoding="utf-8")
        except OSError as exc:
            return _missing(
                "Network denied smoke spec could not be written.",
                _exception_diagnostics(
                    "network_probe_spec_write",
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
                "Network denied smoke failed for sandbox account.",
                _account_runner_launch_exception_diagnostics(
                    "network_probe",
                    exc,
                    state_dir=state_dir,
                    probe_root=root,
                    path=root,
                ),
            )
        ready = (
            result.exit_code == 0 and result.network_denied_verified
            if identity.firewall_blocked
            else result.exit_code == 0 and "network-allowed" in result.stdout
        )
        operation = (
            f"network_probe_{identity.role}"
            if ready
            else f"network_probe_{identity.role}_unexpected_result"
            if result.exit_code == 0
            else _runner_result_operation(f"network_probe_{identity.role}", result)
        )
        return _state_from_bool(
            ready,
            f"Network {identity.network_mode.value} smoke passed for sandbox account.",
            f"Network {identity.network_mode.value} smoke failed for sandbox account.",
            _runner_result_summary(
                operation,
                result,
                state_dir=state_dir,
                probe_root=root,
                path=root,
                extra={"probe": "runtime", "local_user_sid_hash": _hash_sid(sid)},
            ),
        )
    except Exception as exc:
        return _missing(
            "Network denied smoke failed for sandbox account.",
            _account_runner_launch_exception_diagnostics(
                "network_probe",
                exc,
                state_dir=state_dir,
                probe_root=root,
                path=root,
                extra={"probe": "runtime", "local_user_sid_hash": _hash_sid(sid)},
            ),
        )
    finally:
        _cleanup_probe_root(root)

def _host_network_baseline_state() -> WindowsCapabilityState:
    if not _is_windows():
        return _missing("Host outbound connectivity baseline requires Windows.", {"probe": "host_network"})
    state_dir = _windows_state_dir_path()
    failures: list[dict[str, Any]] = []
    for host, port in NETWORK_PROBE_ENDPOINTS:
        try:
            with socket.create_connection((host, int(port)), timeout=2):
                return _state_from_bool(
                    True,
                    "Host outbound connectivity baseline passed.",
                    "Host outbound connectivity baseline failed.",
                    _probe_evidence(
                        "network_probe_host_outbound_baseline",
                        state_dir=state_dir,
                        probe_root=state_dir / "network-smoke",
                        extra={"probe": "host_network", "endpoint_hash": _hash_text(f"{host}:{port}")},
                    ),
                )
        except OSError as exc:
            failures.append(
                _exception_diagnostics(
                    "network_probe_host_outbound_baseline",
                    exc,
                    state_dir=state_dir,
                    probe_root=state_dir / "network-smoke",
                    extra={"probe": "host_network", "endpoint_hash": _hash_text(f"{host}:{port}")},
                )
            )
            continue
    return _missing(
        "Host outbound connectivity baseline failed; cannot prove sandbox firewall denial.",
        _probe_evidence(
            "network_probe_host_outbound_baseline_failed",
            state_dir=state_dir,
            probe_root=state_dir / "network-smoke",
            extra={"probe": "host_network", "attempts": failures},
        ),
    )

def _delete_firewall_rule(name: str) -> _OperationResult:
    if not _is_windows():
        return _OperationResult(False, "Firewall cleanup requires Windows.")
    details = {
        "rule_hash": _hash_text(name),
        "rule_redacted": _redact_account_name(name),
        "group": FIREWALL_RULE_GROUP,
    }
    if not _firewall_rule_exists(name):
        return _OperationResult(True, "firewall_rule_not_present", {"changed": False, **details})
    result = _run_powershell(
        f"Remove-NetFirewallRule -DisplayName {_ps_quote(name)} -ErrorAction Stop"
    )
    if result.returncode == 0:
        return _OperationResult(True, "firewall_rule_removed", {"changed": True, **details})
    return _OperationResult(
        False,
        "Failed to remove sandbox firewall rule.",
        _completed_process_diagnostics(
            "firewall_rule_cleanup",
            result,
            state_dir=_windows_state_dir_path(),
            extra=details,
        ),
    )

def _delete_firewall_group() -> _OperationResult:
    if not _is_windows():
        return _OperationResult(False, "Firewall cleanup requires Windows.")
    count = _firewall_group_rule_count()
    details = {"group": FIREWALL_RULE_GROUP, "rule_count": count}
    if count == 0:
        return _OperationResult(True, "firewall_group_not_present", {"changed": False, **details})
    result = _run_powershell(
        f"Remove-NetFirewallRule -Group {_ps_quote(FIREWALL_RULE_GROUP)} -ErrorAction Stop"
    )
    if result.returncode == 0 and _firewall_group_rule_count() == 0:
        return _OperationResult(True, "firewall_group_removed", {"changed": True, **details})
    return _OperationResult(
        False,
        "Failed to remove Singularity sandbox firewall group.",
        _completed_process_diagnostics(
            "firewall_group_cleanup",
            result,
            state_dir=_windows_state_dir_path(),
            extra=details,
        ),
    )

def _firewall_group_rule_count() -> int:
    if not _is_windows():
        return 0
    result = _run_powershell(
        f"$rules = @(Get-NetFirewallRule -Group {_ps_quote(FIREWALL_RULE_GROUP)} "
        "-ErrorAction SilentlyContinue); $rules.Count"
    )
    if result.returncode != 0:
        return 1
    try:
        return int((result.stdout or "0").strip() or "0")
    except ValueError:
        return 1

def _firewall_rule_exists(name: str) -> bool:
    if not _is_windows():
        return False
    completed = _run_powershell(
        f"if (Get-NetFirewallRule -DisplayName {_ps_quote(name)} -ErrorAction SilentlyContinue) "
        "{ exit 0 }; exit 1"
    )
    return completed.returncode == 0

def _firewall_local_user_sddl(sid: str) -> str:
    return f"D:(A;;CC;;;{sid})"

def _ps_quote(value: str) -> str:
    return "'" + value.replace("'", "''") + "'"
