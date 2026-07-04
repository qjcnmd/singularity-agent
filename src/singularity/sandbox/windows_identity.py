from __future__ import annotations

import singularity.sandbox.windows_common as _windows


def _ensure_sandbox_identity(identity):
    name_error = _windows._validate_sandbox_account_name(identity.account_name)
    if name_error is not None:
        return _windows._OperationResult(
            False,
            name_error["reason"],
            dict(name_error["details"]),
        )
    changed = False
    password = ""
    try:
        if not _windows._account_exists(identity.account_name):
            password = _windows._generate_account_password()
            created = _windows._create_sandbox_account(identity.account_name, password)
            if not created.ok:
                return _windows._OperationResult(
                    False,
                    created.reason,
                    {"phase": "sandbox_accounts", **created.details},
                )
            changed = True
            credential = _windows._store_credential(identity, password)
            if not credential.ok:
                return _windows._OperationResult(
                    False,
                    credential.reason,
                    {"phase": "credentials", **credential.details},
                )
        elif not _windows._credential_state(identity).ready:
            password = _windows._generate_account_password()
            reset = _windows._set_account_password(identity.account_name, password)
            if not reset.ok:
                return _windows._OperationResult(
                    False,
                    reset.reason,
                    {"phase": "credentials", **reset.details},
                )
            credential = _windows._store_credential(identity, password)
            if not credential.ok:
                return _windows._OperationResult(
                    False,
                    credential.reason,
                    {"phase": "credentials", **credential.details},
                )
            changed = True
        return _windows._OperationResult(True, "identity_ready", {"changed": changed})
    finally:
        password = ""
