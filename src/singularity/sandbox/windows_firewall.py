from __future__ import annotations

import singularity.sandbox.windows_common as _windows


def _network_state(sid: str):
    if not _windows._is_windows():
        return _windows._missing(
            "Network filter requires Windows Firewall.",
            {"group": _windows.FIREWALL_RULE_GROUP},
        )
    if not sid:
        return _windows._missing(
            "Network filter requires sandbox account SID.",
            {"group": _windows.FIREWALL_RULE_GROUP},
        )
    sid_literal = _windows._ps_quote(sid)
    rule_name = _windows._ps_quote(_windows.FIREWALL_RULE_NAME)
    command = (
        f"$rule = Get-NetFirewallRule -DisplayName {rule_name} -ErrorAction SilentlyContinue; "
        "if (-not $rule) { exit 1 }; "
        "$security = $rule | Get-NetFirewallSecurityFilter -ErrorAction SilentlyContinue; "
        f"if ($rule.Enabled -eq 'True' -and $rule.Direction -eq 'Outbound' -and "
        f"$rule.Action -eq 'Block' -and ($security.LocalUser -like ('*' + {sid_literal} + '*'))) "
        "{ exit 0 }; exit 1"
    )
    completed = _windows._run_powershell(command)
    return _windows._state_from_bool(
        completed.returncode == 0,
        "Outbound firewall rule is configured.",
        "Outbound firewall rule for sandbox account is missing or incomplete.",
        {
            "rule_hash": _windows._hash_text(_windows.FIREWALL_RULE_NAME),
            "rule_redacted": _windows._redact_account_name(_windows.FIREWALL_RULE_NAME),
            "group": _windows.FIREWALL_RULE_GROUP,
            "local_user_sid_hash": _windows._hash_sid(sid),
        },
    )
