from __future__ import annotations

import ctypes
import re
import secrets
import shutil
import subprocess
import time
from ctypes import wintypes
from typing import Any

from singularity.sandbox.windows_common import (
    _CREDENTIALW,
    _LSA_OBJECT_ATTRIBUTES,
    _LSA_UNICODE_STRING,
    _USER_INFO_1,
    _USER_INFO_1003,
    CRED_PERSIST_LOCAL_MACHINE,
    CRED_TYPE_GENERIC,
    ERROR_INVALID_NAME,
    ERROR_NOT_FOUND,
    LOGIN_UI_USERLIST_KEY,
    NERR_GROUP_NOT_FOUND,
    NERR_INVALID_COMPUTER,
    NERR_INVALID_NAME,
    NERR_SUCCESS,
    NERR_USER_EXISTS,
    NERR_USER_NOT_FOUND,
    POLICY_CREATE_ACCOUNT,
    POLICY_LOOKUP_NAMES,
    SE_BATCH_LOGON_NAME,
    SE_DENY_BATCH_LOGON_NAME,
    SE_DENY_INTERACTIVE_LOGON_NAME,
    SE_DENY_NETWORK_LOGON_NAME,
    SE_DENY_REMOTE_INTERACTIVE_LOGON_NAME,
    SE_DENY_SERVICE_LOGON_NAME,
    SE_INTERACTIVE_LOGON_NAME,
    SE_NETWORK_LOGON_NAME,
    SE_REMOTE_INTERACTIVE_LOGON_NAME,
    SE_SERVICE_LOGON_NAME,
    STATUS_OBJECT_NAME_NOT_FOUND,
    UF_DONT_EXPIRE_PASSWD,
    UF_SCRIPT,
    USER_PRIV_USER,
    WINDOWS_LOCAL_ACCOUNT_NAME_LIMIT,
    WindowsCapabilityState,
    _advapi32,
    _completed_process_diagnostics,
    _hash_text,
    _is_windows,
    _netapi32,
    _OperationResult,
    _run_command,
    _run_powershell,
    _windows_state_dir_path,
    _WindowsSandboxIdentity,
)


def _credential_state(*args, **kwargs):
    from singularity.sandbox.windows_doctor import _credential_state as impl

    return impl(*args, **kwargs)


def _group_membership_state(*args, **kwargs):
    from singularity.sandbox.windows_doctor import _group_membership_state as impl

    return impl(*args, **kwargs)


def _login_ui_visibility_state(*args, **kwargs):
    from singularity.sandbox.windows_doctor import _login_ui_visibility_state as impl

    return impl(*args, **kwargs)


def _logon_rights_state(*args, **kwargs):
    from singularity.sandbox.windows_doctor import _logon_rights_state as impl

    return impl(*args, **kwargs)


def _ps_quote(*args, **kwargs):
    from singularity.sandbox.windows_firewall import _ps_quote as impl

    return impl(*args, **kwargs)


def _ensure_sandbox_identity(identity: _WindowsSandboxIdentity) -> _OperationResult:
    name_error = _validate_sandbox_account_name(identity.account_name)
    if name_error is not None:
        return _OperationResult(
            False,
            name_error["reason"],
            dict(name_error["details"]),
        )
    changed = False
    password = ""
    try:
        if not _account_exists(identity.account_name):
            password = _generate_account_password()
            created = _create_sandbox_account(identity.account_name, password)
            if not created.ok:
                return _OperationResult(
                    False,
                    created.reason,
                    {"phase": "sandbox_accounts", **created.details},
                )
            changed = True
            credential = _store_credential(identity, password)
            if not credential.ok:
                return _OperationResult(
                    False,
                    credential.reason,
                    {"phase": "credentials", **credential.details},
                )
        elif not _credential_state(identity).ready:
            password = _generate_account_password()
            reset = _set_account_password(identity.account_name, password)
            if not reset.ok:
                return _OperationResult(
                    False,
                    reset.reason,
                    {"phase": "credentials", **reset.details},
                )
            credential = _store_credential(identity, password)
            if not credential.ok:
                return _OperationResult(
                    False,
                    credential.reason,
                    {"phase": "credentials", **credential.details},
                )
            changed = True
        return _OperationResult(True, "identity_ready", {"changed": changed})
    finally:
        password = ""


def _setup_identity_security(identity: _WindowsSandboxIdentity) -> _OperationResult:
    sid = _account_sid(identity.account_name)
    if not sid:
        return _OperationResult(False, "sandbox account SID unavailable")
    changed = False
    visibility = _hide_account_from_login_ui(identity.account_name)
    if not visibility.ok:
        return _OperationResult(
            False,
            visibility.reason,
            {"phase": "login_ui_visibility", **visibility.details},
        )
    changed = changed or bool(visibility.details.get("changed"))
    rights = _enumerate_account_logon_rights(sid)
    if not rights.get("interactive"):
        granted = _grant_logon_right(sid)
        if not granted.ok:
            return _OperationResult(
                False,
                granted.reason,
                {"phase": "logon_rights", **granted.details},
            )
        changed = True
    if rights.get("deny_interactive"):
        removed = _remove_deny_logon_rights(sid)
        if not removed.ok:
            return _OperationResult(
                False,
                removed.reason,
                {"phase": "logon_rights", **removed.details},
            )
        changed = True
    hardened = _harden_sandbox_logon_rights(sid)
    if not hardened.ok:
        return _OperationResult(
            False,
            hardened.reason,
            {"phase": "logon_rights", **hardened.details},
        )
    changed = changed or bool(hardened.details.get("changed"))
    post_rights = _enumerate_account_logon_rights(sid)
    if not _logon_rights_state(post_rights).ready:
        return _OperationResult(
            False,
            "sandbox account logon rights were not verified after hardening",
            {"phase": "logon_rights", "logon_rights": post_rights},
        )
    group = _ensure_constrained_group_membership(identity.account_name)
    if not group.ok:
        return _OperationResult(
            False,
            group.reason,
            {"phase": "group_membership", **group.details},
        )
    changed = changed or bool(group.details.get("changed")) or group.reason == "added"
    return _OperationResult(True, "identity_security_ready", {"changed": changed})

def _group_membership_probe_command(account_name: str) -> str:
    return (
        f"$user = Get-LocalUser -Name {_ps_quote(account_name)} -ErrorAction SilentlyContinue; "
        "if (-not $user) { exit 2 }; "
        "$groupSids = @(); "
        "Get-LocalGroup -ErrorAction SilentlyContinue | ForEach-Object { "
        "$group = $_; "
        "$member = Get-LocalGroupMember -Group $group -ErrorAction SilentlyContinue "
        "| Where-Object { $_.SID -eq $user.SID }; "
        "if ($member) { $groupSids += $group.SID.Value } }; "
        "if ($groupSids.Count -eq 1 -and $groupSids[0] -eq 'S-1-5-32-545') "
        "{ exit 0 }; exit 1"
    )

def _ensure_constrained_group_membership(account_name: str) -> _OperationResult:
    if not _is_windows():
        return _OperationResult(False, "Local group hardening requires Windows.")
    before = _group_membership_state(account_name)
    if before.ready:
        return _OperationResult(True, "already_constrained", {"changed": False})
    command = (
        f"$user = Get-LocalUser -Name {_ps_quote(account_name)} -ErrorAction Stop; "
        "$users = Get-LocalGroup -SID 'S-1-5-32-545' -ErrorAction Stop; "
        "Get-LocalGroup -ErrorAction Stop | ForEach-Object { "
        "$group = $_; "
        "$member = Get-LocalGroupMember -Group $group -ErrorAction SilentlyContinue "
        "| Where-Object { $_.SID -eq $user.SID }; "
        "if ($member -and $group.SID.Value -ne 'S-1-5-32-545') { "
        "Remove-LocalGroupMember -Group $group -Member $user -ErrorAction Stop } }; "
        "$member = Get-LocalGroupMember -Group $users -ErrorAction SilentlyContinue "
        "| Where-Object { $_.SID -eq $user.SID }; "
        "if (-not $member) { Add-LocalGroupMember -Group $users -Member $user -ErrorAction Stop }"
    )
    result = _run_powershell(command)
    if result.returncode != 0:
        return _OperationResult(
            False,
            "Failed to constrain sandbox account local group membership.",
            _completed_process_diagnostics(
                "group_membership_harden",
                result,
                state_dir=_windows_state_dir_path(),
                extra={"account": _account_name_diagnostics(account_name)},
            ),
        )
    after = _group_membership_state(account_name)
    return _OperationResult(
        after.ready,
        "group_membership_constrained" if after.ready else after.reason,
        {"changed": True, "state": after.to_dict()},
    )

def _validate_sandbox_account_name(name: str) -> dict[str, Any] | None:
    if len(name) <= WINDOWS_LOCAL_ACCOUNT_NAME_LIMIT:
        return None
    details = _account_name_diagnostics(name)
    details["account_name_limit"] = WINDOWS_LOCAL_ACCOUNT_NAME_LIMIT
    return {
        "step": "sandbox_account",
        "reason": (
            f"Sandbox account name exceeds Windows local user account limit "
            f"({len(name)} > {WINDOWS_LOCAL_ACCOUNT_NAME_LIMIT})."
        ),
        "details": details,
    }

def _account_name_diagnostics(name: str) -> dict[str, Any]:
    return {
        "account_name_length": len(name),
        "account_name_limit": WINDOWS_LOCAL_ACCOUNT_NAME_LIMIT,
        "account_name_hash": _hash_text(name),
        "account_name_redacted": _redact_account_name(name),
    }

def _redact_account_name(name: str) -> str:
    if len(name) <= 6:
        return "*" * len(name)
    return f"{name[:3]}...{name[-3:]}"

def _create_sandbox_account(name: str, password: str) -> _OperationResult:
    if not _is_windows():
        return _OperationResult(False, "Windows account creation requires Windows.")
    name_error = _validate_sandbox_account_name(name)
    if name_error is not None:
        return _OperationResult(False, name_error["reason"], dict(name_error["details"]))
    info = _USER_INFO_1()
    info.usri1_name = name
    info.usri1_password = password
    info.usri1_priv = USER_PRIV_USER
    info.usri1_flags = UF_SCRIPT | UF_DONT_EXPIRE_PASSWD
    param_error = wintypes.DWORD()
    code = _netapi32().NetUserAdd(None, 1, ctypes.byref(info), ctypes.byref(param_error))
    if code in {NERR_SUCCESS, NERR_USER_EXISTS}:
        return _OperationResult(True)
    return _OperationResult(
        False,
        _netapi_error_reason("NetUserAdd", code, param_error.value),
        _netapi_error_details(code, param_error.value),
    )

def _set_account_password(name: str, password: str) -> _OperationResult:
    if not _is_windows():
        return _OperationResult(False, "Windows account password update requires Windows.")
    name_error = _validate_sandbox_account_name(name)
    if name_error is not None:
        return _OperationResult(False, name_error["reason"], dict(name_error["details"]))
    info = _USER_INFO_1003()
    info.usri1003_password = password
    param_error = wintypes.DWORD()
    code = _netapi32().NetUserSetInfo(
        None,
        name,
        1003,
        ctypes.byref(info),
        ctypes.byref(param_error),
    )
    if code == NERR_SUCCESS:
        return _OperationResult(True)
    return _OperationResult(
        False,
        _netapi_error_reason("NetUserSetInfo", code, param_error.value),
        _netapi_error_details(code, param_error.value),
    )

def _delete_sandbox_account(name: str) -> _OperationResult:
    if not _is_windows():
        return _OperationResult(False, "Windows account deletion requires Windows.")
    if not _account_exists(name):
        return _OperationResult(True, "account_not_present", {"changed": False, **_account_name_diagnostics(name)})
    code = _netapi32().NetUserDel(None, name)
    if code == NERR_SUCCESS:
        return _OperationResult(True, "account_deleted", {"changed": True, **_account_name_diagnostics(name)})
    if code == NERR_USER_NOT_FOUND:
        return _OperationResult(True, "account_not_present", {"changed": False, **_account_name_diagnostics(name)})
    details = _netapi_error_details(code, 0)
    details.update(_account_name_diagnostics(name))
    return _OperationResult(False, _netapi_error_reason("NetUserDel", code, 0), details)

def _credential_exists(target: str) -> bool:
    if not _is_windows():
        return False
    credential_ptr = ctypes.c_void_p()
    if not _advapi32().CredReadW(target, CRED_TYPE_GENERIC, 0, ctypes.byref(credential_ptr)):
        return False
    _advapi32().CredFree(credential_ptr)
    return True

def _delete_credential(target: str) -> _OperationResult:
    if not _is_windows():
        return _OperationResult(False, "Windows credential deletion requires Windows.")
    details: dict[str, Any] = {
        "target_hash": _hash_text(target),
        "target_redacted": _redact_account_name(target),
    }
    if not _credential_exists(target):
        return _OperationResult(True, "credential_not_present", {"changed": False, **details})
    if _advapi32().CredDeleteW(target, CRED_TYPE_GENERIC, 0):
        return _OperationResult(True, "credential_deleted", {"changed": True, **details})
    last_error = ctypes.get_last_error()
    if last_error == ERROR_NOT_FOUND:
        return _OperationResult(True, "credential_not_present", {"changed": False, **details})
    details["windows_error_code"] = last_error
    return _OperationResult(False, f"CredDeleteW failed: code {last_error}", details)

def _netapi_error_reason(operation: str, code: int, parm_err: int) -> str:
    explanation = _netapi_error_explanation(code)
    suffix = f" ({explanation})" if explanation else ""
    return f"{operation} failed: code {code}, param {parm_err}{suffix}"

def _netapi_error_details(code: int, parm_err: int) -> dict[str, Any]:
    return {
        "windows_error_code": code,
        "parm_err": parm_err,
        "explanation": _netapi_error_explanation(code),
    }

def _netapi_error_explanation(code: int) -> str:
    if code == NERR_INVALID_NAME:
        return "invalid user/group name parameter"
    if code == NERR_USER_EXISTS:
        return "user already exists"
    if code == NERR_USER_NOT_FOUND:
        return "user not found"
    if code == NERR_GROUP_NOT_FOUND:
        return "group not found"
    if code == ERROR_INVALID_NAME:
        return "invalid name"
    if code == NERR_INVALID_COMPUTER:
        return "invalid computer name"
    return ""

def _store_credential(
    identity: _WindowsSandboxIdentity,
    password: str,
) -> _OperationResult:
    if not _is_windows():
        return _OperationResult(False, "Windows Credential Manager requires Windows.")
    blob = password.encode("utf-16-le")
    blob_buffer = (ctypes.c_ubyte * len(blob)).from_buffer_copy(blob)
    credential = _CREDENTIALW()
    credential.Type = CRED_TYPE_GENERIC
    credential.TargetName = identity.credential_target
    credential.UserName = identity.account_name
    credential.CredentialBlobSize = len(blob)
    credential.CredentialBlob = blob_buffer
    credential.Persist = CRED_PERSIST_LOCAL_MACHINE
    ok = _advapi32().CredWriteW(ctypes.byref(credential), 0)
    if ok:
        return _OperationResult(True)
    code = ctypes.get_last_error()
    return _OperationResult(False, f"CredWriteW failed: code {code}")

def _account_exists(name: str) -> bool:
    return _run_net(["user", name]).returncode == 0

def _run_net(args: list[str]) -> subprocess.CompletedProcess[str]:
    executable = shutil.which("net")
    if executable is None:
        return subprocess.CompletedProcess(["net", *args], 1, "", "net command unavailable")
    return _run_command([executable, *args])

def _account_sid(name: str) -> str:
    if not _is_windows():
        return ""
    completed = _run_powershell(
        f"$u = Get-LocalUser -Name '{name}' -ErrorAction SilentlyContinue; "
        "if ($u) { $u.SID.Value; exit 0 }; exit 1"
    )
    return completed.stdout.strip() if completed.returncode == 0 else ""

def _local_free(ptr: int) -> None:
    if not ptr:
        return
    try:
        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel32.LocalFree.argtypes = [wintypes.LPVOID]
        kernel32.LocalFree.restype = wintypes.LPVOID
        kernel32.LocalFree(ptr)
    except OSError:
        pass

def _account_psid(sid_string: str) -> int:
    """Convert a SID string to a native PSID (caller must _local_free it)."""
    if not _is_windows() or not sid_string:
        return 0
    psid = wintypes.LPVOID()
    if not _advapi32().ConvertStringSidToSidW(sid_string, ctypes.byref(psid)):
        return 0
    return psid.value or 0

def _lsa_open(access: int) -> int:
    attrs = _LSA_OBJECT_ATTRIBUTES()
    attrs.Length = ctypes.sizeof(_LSA_OBJECT_ATTRIBUTES)
    handle = wintypes.HANDLE()
    status = _advapi32().LsaOpenPolicy(None, ctypes.byref(attrs), access, ctypes.byref(handle))
    if status != 0:
        return 0
    return handle.value or 0

def _lsa_close(handle: int) -> None:
    if handle:
        _advapi32().LsaClose(handle)

def _logon_rights_view(rights: list[str], lsa_status: str) -> dict[str, Any]:
    return {
        "interactive": SE_INTERACTIVE_LOGON_NAME in rights,
        "batch": SE_BATCH_LOGON_NAME in rights,
        "network": SE_NETWORK_LOGON_NAME in rights,
        "remote_interactive": SE_REMOTE_INTERACTIVE_LOGON_NAME in rights,
        "service": SE_SERVICE_LOGON_NAME in rights,
        "deny_interactive": SE_DENY_INTERACTIVE_LOGON_NAME in rights,
        "deny_batch": SE_DENY_BATCH_LOGON_NAME in rights,
        "deny_network": SE_DENY_NETWORK_LOGON_NAME in rights,
        "deny_remote_interactive": SE_DENY_REMOTE_INTERACTIVE_LOGON_NAME in rights,
        "deny_service": SE_DENY_SERVICE_LOGON_NAME in rights,
        "rights": sorted(rights),
        "lsa_status": lsa_status,
    }

def _enumerate_account_logon_rights(sid_string: str) -> dict[str, Any]:
    if not _is_windows() or not sid_string:
        return _logon_rights_view([], "not_windows")
    psid = _account_psid(sid_string)
    if not psid:
        return _logon_rights_view([], "sid_lookup_failed")
    try:
        handle = _lsa_open(POLICY_LOOKUP_NAMES)
        if not handle:
            return _logon_rights_view([], "lsa_open_failed")
        array_ptr = ctypes.POINTER(_LSA_UNICODE_STRING)()
        count = wintypes.ULONG(0)
        try:
            status = _advapi32().LsaEnumerateAccountRights(
                handle, psid, ctypes.byref(array_ptr), ctypes.byref(count)
            )
            if status != 0 or not array_ptr:
                return _logon_rights_view(
                    [], f"0x{status & 0xFFFFFFFF:08X}" if status else "empty"
                )
            rights: list[str] = []
            for index in range(count.value):
                entry = array_ptr[index]
                if entry.Length and entry.Buffer:
                    rights.append(ctypes.wstring_at(entry.Buffer, entry.Length // 2))
            return _logon_rights_view(rights, "")
        finally:
            if array_ptr:
                _advapi32().LsaFreeMemory(array_ptr)
            _lsa_close(handle)
    finally:
        _local_free(psid)

def _add_account_rights(sid_string: str, right_names: tuple[str, ...]) -> _OperationResult:
    if not _is_windows():
        return _OperationResult(False, "LSA account right grant requires Windows.")
    if not sid_string:
        return _OperationResult(False, "sandbox account SID unavailable for account right grant")
    if not right_names:
        return _OperationResult(True, "no account rights to add", {"changed": False})
    psid = _account_psid(sid_string)
    if not psid:
        return _OperationResult(False, "sandbox account PSID conversion failed")
    try:
        handle = _lsa_open(POLICY_LOOKUP_NAMES | POLICY_CREATE_ACCOUNT)
        if not handle:
            return _OperationResult(False, "LsaOpenPolicy failed for account right grant")
        try:
            buffers = [ctypes.create_unicode_buffer(name) for name in right_names]
            rights = (_LSA_UNICODE_STRING * len(right_names))()
            for index, name in enumerate(right_names):
                rights[index].Length = len(name) * 2
                rights[index].MaximumLength = (len(name) + 1) * 2
                rights[index].Buffer = ctypes.cast(buffers[index], wintypes.LPWSTR)
            status = _advapi32().LsaAddAccountRights(handle, psid, rights, len(right_names))
            if status != 0:
                return _OperationResult(
                    False,
                    f"LsaAddAccountRights failed: lsa_status=0x{status & 0xFFFFFFFF:08X}",
                    {"rights": list(right_names)},
                )
        finally:
            _lsa_close(handle)
    finally:
        _local_free(psid)
    return _OperationResult(True, f"added {len(right_names)} account right(s)", {"changed": True})

def _remove_account_rights(sid_string: str, right_names: tuple[str, ...]) -> _OperationResult:
    if not _is_windows():
        return _OperationResult(False, "LSA account right removal requires Windows.")
    if not sid_string:
        return _OperationResult(False, "sandbox account SID unavailable for account right removal")
    if not right_names:
        return _OperationResult(True, "no account rights to remove", {"changed": False})
    psid = _account_psid(sid_string)
    if not psid:
        return _OperationResult(False, "sandbox account PSID conversion failed")
    try:
        handle = _lsa_open(POLICY_LOOKUP_NAMES | POLICY_CREATE_ACCOUNT)
        if not handle:
            return _OperationResult(False, "LsaOpenPolicy failed for account right removal")
        try:
            buffers = [ctypes.create_unicode_buffer(name) for name in right_names]
            rights = (_LSA_UNICODE_STRING * len(right_names))()
            for index, name in enumerate(right_names):
                rights[index].Length = len(name) * 2
                rights[index].MaximumLength = (len(name) + 1) * 2
                rights[index].Buffer = ctypes.cast(buffers[index], wintypes.LPWSTR)
            status = _advapi32().LsaRemoveAccountRights(
                handle, psid, False, rights, len(right_names)
            )
            if status != 0:
                return _OperationResult(
                    False,
                    f"LsaRemoveAccountRights failed: lsa_status=0x{status & 0xFFFFFFFF:08X}",
                    {"rights": list(right_names)},
                )
        finally:
            _lsa_close(handle)
    finally:
        _local_free(psid)
    return _OperationResult(True, f"removed {len(right_names)} account right(s)", {"changed": True})

def _remove_all_account_rights(sid_string: str) -> _OperationResult:
    if not _is_windows():
        return _OperationResult(False, "LSA account right removal requires Windows.")
    if not sid_string:
        return _OperationResult(True, "account_sid_not_present", {"changed": False})
    psid = _account_psid(sid_string)
    if not psid:
        return _OperationResult(False, "sandbox account PSID conversion failed")
    try:
        handle = _lsa_open(POLICY_LOOKUP_NAMES | POLICY_CREATE_ACCOUNT)
        if not handle:
            return _OperationResult(False, "LsaOpenPolicy failed for account right removal")
        try:
            status = _advapi32().LsaRemoveAccountRights(handle, psid, True, None, 0)
            normalized_status = status & 0xFFFFFFFF
            if status != 0 and normalized_status != STATUS_OBJECT_NAME_NOT_FOUND:
                return _OperationResult(
                    False,
                    f"LsaRemoveAccountRights failed: lsa_status=0x{normalized_status:08X}",
                )
        finally:
            _lsa_close(handle)
    finally:
        _local_free(psid)
    return _OperationResult(
        True,
        "all_account_rights_removed" if status == 0 else "account_rights_not_present",
        {"changed": status == 0},
    )

def _grant_logon_right(sid_string: str) -> _OperationResult:
    return _add_account_rights(sid_string, (SE_INTERACTIVE_LOGON_NAME,))

def _remove_deny_logon_rights(sid_string: str) -> _OperationResult:
    existing = _enumerate_account_logon_rights(sid_string)
    to_remove = [
        name
        for name, present in (
            (SE_DENY_INTERACTIVE_LOGON_NAME, existing["deny_interactive"]),
        )
        if present
    ]
    if not to_remove:
        return _OperationResult(True, "no conflicting deny logon rights present", {"changed": False})
    return _remove_account_rights(sid_string, tuple(to_remove))

def _harden_sandbox_logon_rights(sid_string: str) -> _OperationResult:
    if not sid_string:
        return _OperationResult(False, "sandbox account SID unavailable for logon hardening")
    existing = _enumerate_account_logon_rights(sid_string)
    allow_remove = [
        name
        for name, present in (
            (SE_BATCH_LOGON_NAME, existing["batch"]),
            (SE_NETWORK_LOGON_NAME, existing["network"]),
            (SE_REMOTE_INTERACTIVE_LOGON_NAME, existing["remote_interactive"]),
            (SE_SERVICE_LOGON_NAME, existing["service"]),
        )
        if present
    ]
    deny_add = [
        name
        for name, present in (
            (SE_DENY_BATCH_LOGON_NAME, existing["deny_batch"]),
            (SE_DENY_NETWORK_LOGON_NAME, existing["deny_network"]),
            (SE_DENY_REMOTE_INTERACTIVE_LOGON_NAME, existing["deny_remote_interactive"]),
            (SE_DENY_SERVICE_LOGON_NAME, existing["deny_service"]),
        )
        if not present
    ]
    remove = _remove_account_rights(sid_string, tuple(allow_remove))
    add = _add_account_rights(sid_string, tuple(deny_add))
    if not remove.ok or not add.ok:
        return _OperationResult(
            False,
            remove.reason if not remove.ok else add.reason,
            {
                "remove": remove.details,
                "add": add.details,
                "removed_rights": allow_remove,
                "added_deny_rights": deny_add,
            },
        )
    post = _enumerate_account_logon_rights(sid_string)
    state = _logon_rights_state(post)
    changed = bool(allow_remove or deny_add or remove.details.get("changed") or add.details.get("changed"))
    return _OperationResult(
        state.ready,
        "logon hardening verified" if state.ready else state.reason,
        {
            "changed": changed,
            "removed_rights": allow_remove,
            "added_deny_rights": deny_add,
            "post_rights": post,
        },
    )

def _hide_account_from_login_ui(account_name: str) -> _OperationResult:
    if not _is_windows():
        return _OperationResult(False, "Login UI visibility hardening requires Windows.")
    before = _login_ui_visibility_state(account_name)
    if before.ready:
        return _OperationResult(True, "already_hidden", {"changed": False})
    result = _run_powershell(
        "$key = "
        f"{_ps_quote(LOGIN_UI_USERLIST_KEY)}; "
        "if (-not (Test-Path -LiteralPath $key)) { "
        "New-Item -Path $key -Force | Out-Null }; "
        f"New-ItemProperty -Path $key -Name {_ps_quote(account_name)} "
        "-Value 0 -PropertyType DWord -Force | Out-Null"
    )
    if result.returncode != 0:
        return _OperationResult(
            False,
            "Failed to hide sandbox account from Windows sign-in user list.",
            _completed_process_diagnostics(
                "login_ui_visibility_harden",
                result,
                state_dir=_windows_state_dir_path(),
                extra={"account": _account_name_diagnostics(account_name)},
            ),
        )
    after = _login_ui_visibility_state(account_name)
    return _OperationResult(
        after.ready,
        "hidden" if after.ready else after.reason,
        {"changed": True, "state": after.to_dict()},
    )

def _stabilize_login_ui_visibility(
    identities: tuple[_WindowsSandboxIdentity, ...],
    *,
    attempts: int = 6,
    interval_seconds: float = 1.0,
) -> _OperationResult:
    changed = False
    last_states: dict[str, WindowsCapabilityState] = {}
    for _attempt in range(max(1, attempts)):
        for identity in identities:
            hidden = _hide_account_from_login_ui(identity.account_name)
            changed = changed or bool(hidden.details.get("changed"))
            if not hidden.ok:
                return _OperationResult(
                    False,
                    hidden.reason,
                    {"changed": changed, "role": identity.role, **hidden.details},
                )
        if interval_seconds > 0:
            time.sleep(interval_seconds)
        last_states = {
            identity.role: _login_ui_visibility_state(identity.account_name)
            for identity in identities
        }
        if all(state.ready for state in last_states.values()):
            return _OperationResult(
                True,
                "login_ui_visibility_stable",
                {"changed": changed},
            )
    return _OperationResult(
        False,
        "Sandbox account login UI visibility did not remain stable after setup probes.",
        {
            "changed": changed,
            "states": {role: state.to_dict() for role, state in last_states.items()},
        },
    )

def _remove_login_ui_visibility_entry(account_name: str) -> _OperationResult:
    if not _is_windows():
        return _OperationResult(False, "Login UI visibility cleanup requires Windows.")
    details = {
        "registry_key_hash": _hash_text(LOGIN_UI_USERLIST_KEY),
        "account": _account_name_diagnostics(account_name),
    }
    result = _run_powershell(
        "$key = "
        f"{_ps_quote(LOGIN_UI_USERLIST_KEY)}; "
        f"$name = {_ps_quote(account_name)}; "
        "$value = Get-ItemPropertyValue -LiteralPath $key -Name $name "
        "-ErrorAction SilentlyContinue; if ($null -eq $value) { exit 2 }; "
        "Remove-ItemProperty -LiteralPath $key -Name $name -ErrorAction Stop"
    )
    if result.returncode in {0, 2}:
        return _OperationResult(
            True,
            "login_ui_visibility_removed" if result.returncode == 0 else "login_ui_visibility_not_present",
            {"changed": result.returncode == 0, **details},
        )
    return _OperationResult(
        False,
        "Failed to remove sandbox login UI visibility entry.",
        _completed_process_diagnostics(
            "login_ui_visibility_cleanup",
            result,
            state_dir=_windows_state_dir_path(),
            extra=details,
        ),
    )

def _generate_account_password() -> str:
    return "Sg!" + secrets.token_urlsafe(32) + "9"

def _current_process_sid() -> str:
    if not _is_windows():
        return ""
    result = _run_powershell(
        "[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value"
    )
    value = (result.stdout or "").strip()
    if result.returncode != 0 or re.fullmatch(r"S-\d+(?:-\d+)+", value) is None:
        return ""
    return value
