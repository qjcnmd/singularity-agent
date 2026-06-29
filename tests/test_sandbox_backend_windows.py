import json
import subprocess
import sys
from dataclasses import replace
from datetime import UTC, datetime
from pathlib import Path

import pytest
from typer.testing import CliRunner

import singularity.sandbox as sandbox
import singularity.sandbox.windows as windows
from singularity.cli import app
from singularity.sandbox import (
    PreparedSandbox,
    SandboxCapabilityError,
    SandboxProfileName,
    SandboxRequest,
    SandboxStatus,
)


def _request(tmp_path: Path) -> SandboxRequest:
    return SandboxRequest(
        sandbox_id="sandbox_windows",
        session_id="session",
        task_id="task",
        action_id="action",
        command=["python", "-c", "print('must-not-run')"],
        cwd=tmp_path,
        workspace_root=tmp_path,
        profile=sandbox.default_sandbox_profile(
            SandboxProfileName.ISOLATED_VERIFICATION,
            workspace_root=tmp_path,
        ),
    )


def test_windows_doctor_reports_primitives_and_setup_separately() -> None:
    report = sandbox.probe_windows_sandbox()
    payload = report.to_dict()

    assert payload["schema_version"] == "sandbox.windows.doctor/v2"
    assert payload["implementation"] == "elevated"
    assert "status" not in payload
    assert payload["enforcement_status"] in {
        "available",
        "backend_unavailable",
        "not_supported",
    }
    assert set(payload["primitives"]) == {
        "restricted_token",
        "job_object",
        "low_integrity",
        "acl",
        "firewall",
        "private_desktop",
    }
    for item in payload["primitives"].values():
        assert set(item) == {"status", "checked", "reason", "evidence"}
        assert item["status"] in {"available", "missing", "not_supported"}
        assert item["checked"] is True
        assert "secret" not in str(item["evidence"]).lower()
    assert set(payload["setup"]) == {
        "sandbox_accounts",
        "login_ui_visibility",
        "logon_rights",
        "group_membership",
        "state_dir_acl",
        "acl_boundary",
        "offline_network_filter",
        "private_desktop",
        "execution_backends",
        "legacy_assets",
    }
    assert set(payload["execution"]) == {
        "account_sids",
        "credentials",
        "launchers",
        "runner_smoke",
        "network_probe",
    }
    assert isinstance(payload["blocking_requirements"], list)
    assert "recommended_action" in payload
    assert isinstance(payload["diagnostics"], list)
    assert report.available == (payload["enforcement_status"] == "available")
    assert report.missing_requirements == tuple(payload["blocking_requirements"])


def test_windows_backend_is_unavailable_until_all_enforcement_is_configured() -> None:
    backend = sandbox.WindowsSandboxBackend()
    report = backend.doctor()

    if report.available:
        assert backend.is_available() is True
        assert backend.capabilities().filesystem_isolation is True
        assert backend.capabilities().network_isolation is True
    else:
        assert report.blocking_requirements
        assert backend.is_available() is False
        assert backend.capabilities().filesystem_isolation is False
        assert backend.capabilities().network_isolation is False


def test_windows_setup_reports_machine_readable_status() -> None:
    backend = sandbox.WindowsSandboxBackend()

    report = backend.setup()
    payload = report.to_dict()

    assert payload["schema_version"] == "sandbox.windows.setup/v2"
    assert payload["status"] in {
        "not_supported",
        "requires_elevation",
        "partial",
        "ready",
        "failed",
    }
    assert isinstance(payload["requires_elevation"], bool)
    assert isinstance(payload["changed"], bool)
    assert isinstance(payload["completed_steps"], list)
    assert isinstance(payload["pending_steps"], list)
    assert isinstance(payload["failed_steps"], list)
    assert isinstance(payload["available_after_setup"], bool)
    assert payload["message"]


def test_sandbox_setup_cli_does_not_rewrite_partial_status(monkeypatch: pytest.MonkeyPatch) -> None:
    report = sandbox.WindowsSandboxSetupReport(
        status="requires_elevation",
        requested_operation="setup",
        requires_elevation=True,
        changed=False,
        completed_steps=(),
        pending_steps=("sandbox_account",),
        failed_steps=(),
        available_after_setup=False,
        message="elevation required",
    )
    monkeypatch.setattr(sandbox.WindowsSandboxBackend, "setup", lambda self: report)

    result = CliRunner().invoke(app, ["sandbox", "setup", "--json"])

    assert result.exit_code == 1
    payload = __import__("json").loads(result.stdout)
    assert payload["schema_version"] == "sandbox.windows.setup/v2"
    assert payload["status"] == "requires_elevation"
    assert payload["available_after_setup"] is False


def test_sandbox_cleanup_cli_reports_machine_readable_status(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    report = sandbox.WindowsSandboxCleanupReport(
        status="completed",
        requested_operation="cleanup",
        requires_elevation=False,
        changed=True,
        completed_steps=("state_dir",),
        failed_steps=(),
        diagnostics=(),
        residual_audit={
            "accounts": 0,
            "credentials": 0,
            "firewall_rules": 0,
            "login_ui_entries": 0,
            "security_attestations": 0,
            "state_dirs": 0,
        },
    )
    monkeypatch.setattr(sandbox.WindowsSandboxBackend, "cleanup_assets", lambda self: report)

    result = CliRunner().invoke(app, ["sandbox", "cleanup", "--json"])

    assert result.exit_code == 0
    payload = json.loads(result.stdout)
    assert payload["schema_version"] == "sandbox.windows.cleanup/v2"
    assert payload["requested_operation"] == "cleanup"
    assert payload["status"] == "completed"
    assert payload["completed"] is True
    assert payload["failed"] is False
    assert payload["changed"] is True
    assert all(count == 0 for count in payload["residual_audit"].values())


def test_windows_account_probe_handles_missing_net_command_without_name_error(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(windows.shutil, "which", lambda _name: None)

    assert windows._account_exists(windows.OFFLINE_SANDBOX_ACCOUNT) is False


def test_windows_sandbox_account_names_fit_local_user_limit_and_match_network_modes() -> None:
    assert windows.WINDOWS_LOCAL_ACCOUNT_NAME_LIMIT == 20
    assert windows.OFFLINE_SANDBOX_ACCOUNT == "SingularityOffline"
    assert windows.ONLINE_SANDBOX_ACCOUNT == "SingularityOnline"
    assert all(
        len(name) <= windows.WINDOWS_LOCAL_ACCOUNT_NAME_LIMIT
        for name in windows.SANDBOX_ACCOUNTS
    )

    offline = windows._sandbox_identity_for_mode(sandbox.SandboxNetworkMode.DENIED)
    online = windows._sandbox_identity_for_mode(sandbox.SandboxNetworkMode.ALLOWED)

    assert offline.account_name == windows.OFFLINE_SANDBOX_ACCOUNT
    assert offline.credential_target == windows.OFFLINE_SANDBOX_ACCOUNT
    assert offline.firewall_blocked is True
    assert online.account_name == windows.ONLINE_SANDBOX_ACCOUNT
    assert online.credential_target == windows.ONLINE_SANDBOX_ACCOUNT
    assert online.firewall_blocked is False

    with pytest.raises(SandboxCapabilityError, match="network mode allowlist"):
        windows._sandbox_identity_for_mode(sandbox.SandboxNetworkMode.ALLOWLIST)


def test_windows_setup_rejects_oversized_account_name_before_create(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    attempted: list[str] = []

    def fail_create(name: str, _password: str) -> windows._OperationResult:
        attempted.append(name)
        raise AssertionError("setup must reject oversized account names before NetUserAdd")

    monkeypatch.setattr(windows.os, "name", "nt", raising=False)
    monkeypatch.setattr(windows, "_create_sandbox_account", fail_create)
    identity = windows._WindowsSandboxIdentity(
        role="offline",
        network_mode=sandbox.SandboxNetworkMode.DENIED,
        account_name="SingularitySandboxRunner",
        credential_target="SingularitySandboxRunner",
        firewall_blocked=True,
    )

    result = windows._ensure_sandbox_identity(identity)

    assert attempted == []
    assert result.ok is False
    assert "exceeds Windows local user account limit" in result.reason
    assert result.details["account_name_length"] == len("SingularitySandboxRunner")
    assert result.details["account_name_limit"] == 20
    assert result.details["account_name_hash"]
    assert result.details["account_name_redacted"].startswith("Sin")


def test_windows_cleanup_deletes_oversized_legacy_account_without_create_name_validation(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    deleted: list[str] = []

    class FakeNetApi:
        def NetUserDel(self, _server, name: str) -> int:
            deleted.append(name)
            return windows.NERR_SUCCESS

    monkeypatch.setattr(windows.os, "name", "nt", raising=False)
    monkeypatch.setattr(windows, "_account_exists", lambda _name: True)
    monkeypatch.setattr(windows, "_netapi32", lambda: FakeNetApi())

    result = windows._delete_sandbox_account(windows.LEGACY_SANDBOX_ACCOUNT)

    assert result.ok is True
    assert result.reason == "account_deleted"
    assert deleted == [windows.LEGACY_SANDBOX_ACCOUNT]


def test_windows_setup_reports_netuseradd_2202_diagnostics(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    class FakeNetApi:
        def NetUserAdd(self, _server, _level, _buffer, parm_err) -> int:
            parm_err._obj.value = 0
            return 2202

    monkeypatch.setattr(windows.os, "name", "nt", raising=False)
    monkeypatch.setattr(windows, "_account_exists", lambda _name: False)
    monkeypatch.setattr(windows, "_netapi32", lambda: FakeNetApi())

    result = windows._ensure_sandbox_identity(
        windows._sandbox_identity_for_mode(sandbox.SandboxNetworkMode.DENIED)
    )

    assert "NetUserAdd failed: code 2202" in result.reason
    assert "invalid user/group name parameter" in result.reason
    assert result.details["windows_error_code"] == 2202
    assert result.details["parm_err"] == 0


def test_windows_doctor_and_setup_message_report_legacy_artifacts(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    ready = sandbox.WindowsCapabilityState("available", True, "ready", {})
    legacy = (
        {
            "kind": "legacy_sandbox_account",
            "status": "present",
            "account_name_length": len("SingularitySandboxRunner"),
            "account_name_hash": "legacy_hash",
            "account_name_redacted": "Sin...ner",
        },
    )

    monkeypatch.setattr(windows.os, "name", "nt", raising=False)
    monkeypatch.setattr(windows, "_legacy_artifact_diagnostics", lambda: legacy)
    monkeypatch.setattr(windows, "_primitive", lambda *_args: ready)
    monkeypatch.setattr(windows, "_command_state", lambda *_args: ready)
    monkeypatch.setattr(windows, "_powershell_state", lambda *_args: ready)
    monkeypatch.setattr(windows, "_account_sid", lambda _name: "S-1-5-21-123")
    monkeypatch.setattr(windows, "_acl_state", lambda *_args: ready)
    monkeypatch.setattr(windows, "_network_state", lambda _sid: ready)
    monkeypatch.setattr(windows, "_online_network_filter_state", lambda _sid: ready)
    monkeypatch.setattr(windows, "_execution_backend_state", lambda *_args: ready)
    monkeypatch.setattr(windows, "_credential_state", lambda *_args: ready)
    monkeypatch.setattr(windows, "_runner_smoke_state", lambda *_args: ready)
    monkeypatch.setattr(windows, "_network_probe_state", lambda *_args: ready)
    monkeypatch.setattr(windows, "_login_ui_visibility_state", lambda *_args: ready)
    monkeypatch.setattr(windows, "_security_attestation_state", lambda *_args: ready)
    monkeypatch.setattr(windows, "_logon_rights_state", lambda *_args, **_kwargs: ready)
    monkeypatch.setattr(windows, "_group_membership_state", lambda *_args: ready)
    monkeypatch.setattr(windows, "_state_dir_state", lambda: ready)
    monkeypatch.setattr(windows, "_state_dir_acl_state", lambda: ready)
    monkeypatch.setattr(windows, "_launcher_state", lambda *_args: ready)

    doctor = windows._probe_windows_sandbox_uncached()

    assert doctor.diagnostics == legacy
    assert doctor.to_dict()["diagnostics"] == list(legacy)
    assert "legacy sandbox artifacts detected" in doctor.recommended_action.lower()

    assert "legacy sandbox artifacts detected" in windows._setup_message(doctor, legacy).lower()


def test_windows_legacy_artifact_diagnostics_report_old_account_credential_and_firewall(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(windows.os, "name", "nt", raising=False)
    monkeypatch.setattr(
        windows,
        "_account_exists",
        lambda name: name == windows.LEGACY_SANDBOX_ACCOUNT,
    )
    monkeypatch.setattr(
        windows,
        "_credential_exists",
        lambda target: target == windows.LEGACY_SANDBOX_ACCOUNT,
    )
    monkeypatch.setattr(
        windows,
        "_firewall_rule_exists",
        lambda name: name == windows.LEGACY_FIREWALL_RULE_NAME,
    )

    diagnostics = windows._legacy_artifact_diagnostics()

    kinds = {item["kind"] for item in diagnostics}
    assert kinds == {"legacy_sandbox_account", "legacy_credential", "legacy_firewall_rule"}
    account = next(item for item in diagnostics if item["kind"] == "legacy_sandbox_account")
    assert account["account_name_length"] == len(windows.LEGACY_SANDBOX_ACCOUNT)
    assert account["account_name_redacted"].startswith("Sin")
    assert account["account_name_hash"]
    credential = next(item for item in diagnostics if item["kind"] == "legacy_credential")
    assert credential["target_redacted"].startswith("Sin")
    assert credential["target_hash"]
    firewall = next(item for item in diagnostics if item["kind"] == "legacy_firewall_rule")
    assert firewall["rule_redacted"].startswith("Sin")
    assert firewall["rule_hash"]
    assert firewall["group"] == windows.FIREWALL_RULE_GROUP


def test_windows_cleanup_requires_elevation_before_destructive_actions(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(windows.os, "name", "nt", raising=False)
    monkeypatch.setattr(windows, "_is_elevated", lambda: False)
    for name in (
        "_delete_sandbox_account",
        "_delete_credential",
        "_delete_firewall_rule",
        "_delete_windows_state_dir",
        "_remove_login_ui_visibility_entry",
    ):
        monkeypatch.setattr(
            windows,
            name,
            lambda *args, _name=name, **kwargs: (_ for _ in ()).throw(
                AssertionError(f"{_name} must not run without elevation")
            ),
        )

    report = windows.cleanup_windows_sandbox_assets()

    assert report.status == "requires_elevation"
    assert report.requires_elevation is True
    assert report.changed is False


def test_windows_cleanup_removes_current_and_legacy_assets_idempotently(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    calls: list[tuple[str, str]] = []

    def changed_result(kind: str, target: str) -> windows._OperationResult:
        calls.append((kind, target))
        return windows._OperationResult(True, "removed", {"changed": target == windows.LEGACY_SANDBOX_ACCOUNT})

    monkeypatch.setattr(windows.os, "name", "nt", raising=False)
    monkeypatch.setattr(windows, "_is_elevated", lambda: True)
    monkeypatch.setattr(
        windows,
        "_legacy_artifact_diagnostics",
        lambda: (
            {
                "kind": "legacy_sandbox_account",
                "status": "present",
                "account_name_hash": "legacy",
                "account_name_redacted": "Sin...ner",
            },
        ),
    )
    monkeypatch.setattr(windows, "_delete_credential", lambda target: changed_result("credential", target))
    monkeypatch.setattr(
        windows,
        "_delete_firewall_group",
        lambda: windows._OperationResult(True, "removed", {"changed": False}),
    )
    monkeypatch.setattr(
        windows,
        "_delete_security_attestation",
        lambda: windows._OperationResult(True, "not_present", {"changed": False}),
    )
    monkeypatch.setattr(
        windows,
        "_remove_login_ui_visibility_entry",
        lambda target: changed_result("visibility", target),
    )
    monkeypatch.setattr(
        windows,
        "_delete_windows_state_dir",
        lambda: windows._OperationResult(True, "not_present", {"changed": False}),
    )
    monkeypatch.setattr(windows, "_delete_sandbox_account", lambda target: changed_result("account", target))
    monkeypatch.setattr(windows, "_account_sid", lambda _target: "")
    monkeypatch.setattr(
        windows,
        "_sandbox_residual_audit",
        lambda: {
            "accounts": 0,
            "credentials": 0,
            "firewall_rules": 0,
            "login_ui_entries": 0,
            "security_attestations": 0,
            "state_dirs": 0,
        },
    )

    report = windows.cleanup_windows_sandbox_assets()
    payload = report.to_dict()
    text = json.dumps(payload)

    assert report.status == "completed"
    assert report.changed is True
    assert {kind for kind, _target in calls} == {"credential", "visibility", "account"}
    assert ( "account", windows.LEGACY_SANDBOX_ACCOUNT) in calls
    assert ("account", windows.OFFLINE_SANDBOX_ACCOUNT) in calls
    assert ("account", windows.ONLINE_SANDBOX_ACCOUNT) in calls
    assert payload["schema_version"] == "sandbox.windows.cleanup/v2"
    assert payload["failed_steps"] == []
    assert "S-1-5-21" not in text
    assert "password" not in text.lower()


def test_windows_cleanup_fails_closed_when_residual_audit_finds_account(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    no_change = windows._OperationResult(True, "not_present", {"changed": False})
    monkeypatch.setattr(windows.os, "name", "nt", raising=False)
    monkeypatch.setattr(windows, "_is_elevated", lambda: True)
    monkeypatch.setattr(windows, "_legacy_artifact_diagnostics", lambda: ())
    monkeypatch.setattr(windows, "_delete_credential", lambda _target: no_change)
    monkeypatch.setattr(windows, "_delete_firewall_group", lambda: no_change)
    monkeypatch.setattr(windows, "_delete_security_attestation", lambda: no_change)
    monkeypatch.setattr(windows, "_remove_login_ui_visibility_entry", lambda _target: no_change)
    monkeypatch.setattr(windows, "_delete_windows_state_dir", lambda: no_change)
    monkeypatch.setattr(windows, "_account_sid", lambda _target: "")
    monkeypatch.setattr(windows, "_delete_sandbox_account", lambda _target: no_change)
    monkeypatch.setattr(
        windows,
        "_sandbox_residual_audit",
        lambda: {
            "accounts": 1,
            "credentials": 0,
            "firewall_rules": 0,
            "login_ui_entries": 0,
            "security_attestations": 0,
            "state_dirs": 0,
        },
    )

    report = windows.cleanup_windows_sandbox_assets()

    assert report.status == "failed"
    assert report.changed is False
    assert report.residual_audit["accounts"] == 1
    assert any(step["step"] == "residual_audit" for step in report.failed_steps)


def test_windows_state_dir_cleanup_repairs_acl_integrity_and_attributes_before_delete(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    state_dir = tmp_path / "Singularity" / "windows-sandbox"
    state_dir.mkdir(parents=True)
    readonly_file = state_dir / "readonly.txt"
    readonly_file.write_text("data", encoding="utf-8")
    commands: list[list[str]] = []

    def fake_run(command: list[str]) -> subprocess.CompletedProcess[str]:
        commands.append(command)
        return subprocess.CompletedProcess(command, 0, "", "")

    monkeypatch.setattr(windows.os, "name", "nt", raising=False)
    monkeypatch.setattr(windows, "_windows_state_dir_path", lambda: state_dir)
    monkeypatch.setattr(windows.shutil, "which", lambda name: f"{name}.exe")
    monkeypatch.setattr(windows, "_run_command", fake_run)

    result = windows._delete_windows_state_dir()

    flattened = [" ".join(command).lower() for command in commands]
    assert result.ok is True
    assert state_dir.exists() is False
    assert any("takeown" in command for command in flattened)
    assert any("icacls" in command and "/reset" in command for command in flattened)
    assert any("icacls" in command and "/setintegritylevel" in command for command in flattened)
    assert any("attrib" in command and "-r" in command for command in flattened)


def test_login_ui_visibility_state_requires_hidden_userlist_entry(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    calls: list[str] = []

    def fake_powershell(command: str) -> subprocess.CompletedProcess[str]:
        calls.append(command)
        return subprocess.CompletedProcess(["powershell"], 0, "", "")

    monkeypatch.setattr(windows.os, "name", "nt", raising=False)
    monkeypatch.setattr(windows, "_run_powershell", fake_powershell)

    state = windows._login_ui_visibility_state(windows.OFFLINE_SANDBOX_ACCOUNT)
    payload = json.dumps(state.to_dict())

    assert state.status == "available"
    assert "SpecialAccounts" not in payload
    assert windows.OFFLINE_SANDBOX_ACCOUNT not in payload
    assert "registry_key_hash" in state.evidence
    assert calls and "UserList" in calls[0]


def test_hide_account_from_login_ui_preserves_existing_userlist_key(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    calls: list[str] = []
    states = iter(
        (
            sandbox.WindowsCapabilityState("missing", True, "missing", {}),
            sandbox.WindowsCapabilityState("available", True, "hidden", {}),
        )
    )

    def fake_powershell(command: str) -> subprocess.CompletedProcess[str]:
        calls.append(command)
        return subprocess.CompletedProcess(["powershell"], 0, "", "")

    monkeypatch.setattr(windows.os, "name", "nt", raising=False)
    monkeypatch.setattr(windows, "_login_ui_visibility_state", lambda _account: next(states))
    monkeypatch.setattr(windows, "_run_powershell", fake_powershell)

    result = windows._hide_account_from_login_ui(windows.OFFLINE_SANDBOX_ACCOUNT)

    assert result.ok is True
    assert len(calls) == 1
    assert "if (-not (Test-Path -LiteralPath $key))" in calls[0]


def test_login_ui_visibility_stabilization_reapplies_after_transient_missing_entry(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    ready = sandbox.WindowsCapabilityState("available", True, "hidden", {})
    missing = sandbox.WindowsCapabilityState("missing", True, "missing", {})
    checks = iter(
        (
            {"offline": missing, "online": ready},
            {"offline": ready, "online": ready},
        )
    )
    hidden: list[str] = []
    current: dict[str, sandbox.WindowsCapabilityState] = {}

    def hide(account: str) -> windows._OperationResult:
        hidden.append(account)
        return windows._OperationResult(True, "hidden", {"changed": True})

    def state(account: str) -> sandbox.WindowsCapabilityState:
        if not current:
            current.update(next(checks))
        role = "offline" if account == windows.OFFLINE_SANDBOX_ACCOUNT else "online"
        result = current[role]
        if role == "online":
            current.clear()
        return result

    monkeypatch.setattr(windows, "_hide_account_from_login_ui", hide)
    monkeypatch.setattr(windows, "_login_ui_visibility_state", state)
    monkeypatch.setattr(windows.time, "sleep", lambda _seconds: None)

    result = windows._stabilize_login_ui_visibility(
        tuple(windows._SANDBOX_IDENTITIES.values()),
        attempts=2,
        interval_seconds=0,
    )

    assert result.ok is True
    assert hidden.count(windows.OFFLINE_SANDBOX_ACCOUNT) == 2
    assert hidden.count(windows.ONLINE_SANDBOX_ACCOUNT) == 2


def test_logon_rights_state_requires_hardened_deny_surface() -> None:
    hardened = windows._logon_rights_view(
        [
            "SeInteractiveLogonRight",
            "SeDenyRemoteInteractiveLogonRight",
            "SeDenyNetworkLogonRight",
            "SeDenyServiceLogonRight",
            "SeDenyBatchLogonRight",
        ],
        "",
    )
    overexposed = windows._logon_rights_view(
        [
            "SeInteractiveLogonRight",
            "SeNetworkLogonRight",
            "SeDenyRemoteInteractiveLogonRight",
            "SeDenyServiceLogonRight",
            "SeDenyBatchLogonRight",
        ],
        "",
    )

    assert windows._logon_rights_state(hardened).status == "available"
    state = windows._logon_rights_state(overexposed)
    assert state.status == "missing"
    assert state.evidence["logon_rights"]["network"] is True
    assert "CreateProcessWithLogonW" in state.evidence["interactive_logon_note"]


def test_logon_rights_state_uses_protected_attestation_only_for_lsa_access_denied() -> None:
    access_denied = windows._logon_rights_view([], "0xC0000022")
    other_error = windows._logon_rights_view([], "0xC0000001")

    assert windows._logon_rights_state(access_denied, attested=False).status == "missing"
    attested = windows._logon_rights_state(access_denied, attested=True)
    assert attested.status == "available"
    assert attested.evidence["verification_source"] == "protected_setup_attestation"
    assert windows._logon_rights_state(other_error, attested=True).status == "missing"


def test_security_attestation_requires_matching_sid_hashes_and_protected_acl(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    payload = {
        "schema_version": windows.SECURITY_ATTESTATION_SCHEMA_VERSION,
        "policy": windows.SECURITY_ATTESTATION_POLICY,
        "principals": {
            "offline": windows._hash_sid("offline-sid"),
            "online": windows._hash_sid("online-sid"),
        },
        "acl_protected": True,
    }
    monkeypatch.setattr(windows.os, "name", "nt", raising=False)
    monkeypatch.setattr(
        windows,
        "_run_powershell",
        lambda _command: subprocess.CompletedProcess(
            ["powershell"], 0, json.dumps(payload), ""
        ),
    )

    state = windows._security_attestation_state(
        {"offline": "offline-sid", "online": "online-sid"}
    )
    mismatch = windows._security_attestation_state(
        {"offline": "different-sid", "online": "online-sid"}
    )

    assert state.status == "available"
    assert state.evidence["principal_count"] == 2
    assert mismatch.status == "missing"
    assert "offline-sid" not in json.dumps(state.to_dict())


def test_legacy_cleanup_skips_acl_removal_for_absent_legacy_accounts(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    state_dir = tmp_path / "windows-sandbox"
    state_dir.mkdir()
    no_change = windows._OperationResult(True, "not_present", {"changed": False})
    monkeypatch.setattr(windows, "_delete_credential", lambda _target: no_change)
    monkeypatch.setattr(windows, "_remove_login_ui_visibility_entry", lambda _target: no_change)
    monkeypatch.setattr(windows, "_delete_firewall_rule", lambda _target: no_change)
    monkeypatch.setattr(windows, "_windows_state_dir_path", lambda: state_dir)
    monkeypatch.setattr(windows, "_account_exists", lambda _target: False)
    monkeypatch.setattr(windows, "_legacy_artifact_diagnostics", lambda: ())
    monkeypatch.setattr(windows.shutil, "which", lambda _name: "icacls.exe")
    monkeypatch.setattr(
        windows,
        "_run_command",
        lambda _command: (_ for _ in ()).throw(
            AssertionError("absent legacy accounts must be an ACL cleanup no-op")
        ),
    )

    result = windows._cleanup_legacy_assets()

    assert result.ok is True
    assert result.details["changed"] is False


def test_credential_state_redacts_target_name(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(windows.os, "name", "nt", raising=False)
    monkeypatch.setattr(windows, "_credential_exists", lambda _target: True)

    identity = windows._sandbox_identity_for_mode(sandbox.SandboxNetworkMode.DENIED)
    state = windows._credential_state(identity)
    text = json.dumps(state.to_dict())

    assert state.status == "available"
    assert "target_hash" in state.evidence
    assert "target_redacted" in state.evidence
    assert identity.credential_target not in text


def test_network_state_redacts_firewall_rule_name(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(windows.os, "name", "nt", raising=False)
    monkeypatch.setattr(
        windows,
        "_run_powershell",
        lambda _command: subprocess.CompletedProcess(["powershell"], 0, "", ""),
    )

    state = windows._network_state("S-1-5-21-123")
    text = json.dumps(state.to_dict())

    assert state.status == "available"
    assert state.evidence["rule_hash"]
    assert state.evidence["rule_redacted"]
    assert windows.FIREWALL_RULE_NAME not in text
    assert "S-1-5-21" not in text


def test_setup_logon_hardening_removes_unneeded_allows_and_adds_denies(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    calls: list[tuple[str, tuple[str, ...]]] = []

    def fake_remove(_sid: str, rights: tuple[str, ...]) -> windows._OperationResult:
        calls.append(("remove", rights))
        return windows._OperationResult(True, "removed", {"changed": bool(rights)})

    def fake_add(_sid: str, rights: tuple[str, ...]) -> windows._OperationResult:
        calls.append(("add", rights))
        return windows._OperationResult(True, "added", {"changed": bool(rights)})

    states = [
        windows._logon_rights_view(
            [
                "SeInteractiveLogonRight",
                "SeNetworkLogonRight",
                "SeBatchLogonRight",
            ],
            "",
        ),
        windows._logon_rights_view(
            [
                "SeInteractiveLogonRight",
                "SeDenyRemoteInteractiveLogonRight",
                "SeDenyNetworkLogonRight",
                "SeDenyServiceLogonRight",
                "SeDenyBatchLogonRight",
            ],
            "",
        ),
    ]

    monkeypatch.setattr(windows, "_enumerate_account_logon_rights", lambda _sid: states.pop(0))
    monkeypatch.setattr(windows, "_remove_account_rights", fake_remove)
    monkeypatch.setattr(windows, "_add_account_rights", fake_add)

    result = windows._harden_sandbox_logon_rights("S-1-5-21-123")

    assert result.ok is True
    assert result.details["changed"] is True
    assert ("remove", ("SeBatchLogonRight", "SeNetworkLogonRight")) in calls
    added = next(rights for action, rights in calls if action == "add")
    assert set(added) == {
        "SeDenyBatchLogonRight",
        "SeDenyNetworkLogonRight",
        "SeDenyRemoteInteractiveLogonRight",
        "SeDenyServiceLogonRight",
    }


def test_windows_setup_requires_elevation_before_system_mutation(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    mutations = [
        "_account_exists",
        "_create_sandbox_account",
        "_store_credential",
        "_run_powershell",
        "_apply_account_acl",
    ]

    monkeypatch.setattr(windows.os, "name", "nt", raising=False)
    monkeypatch.setattr(windows, "_is_elevated", lambda: False)
    for name in mutations:
        monkeypatch.setattr(
            windows,
            name,
            lambda *args, _name=name, **kwargs: (_ for _ in ()).throw(
                AssertionError(f"{_name} must not run without elevation")
            ),
        )

    report = windows.setup_windows_sandbox()

    assert report.status == "requires_elevation"
    assert report.requires_elevation is True
    assert report.changed is False
    assert report.completed_steps == ()
    assert set(report.pending_steps) == set(windows.SETUP_STEP_ORDER)


def test_windows_setup_elevated_provisions_and_verifies_both_accounts(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    calls: list[tuple[str, str]] = []
    ready = sandbox.WindowsCapabilityState("available", True, "ready", {})
    network_calls = 0

    def ensure_identity(identity: windows._WindowsSandboxIdentity) -> windows._OperationResult:
        calls.append(("identity", identity.role))
        return windows._OperationResult(True, "ready", {"changed": True})

    def secure_identity(identity: windows._WindowsSandboxIdentity) -> windows._OperationResult:
        calls.append(("security", identity.role))
        return windows._OperationResult(True, "ready", {"changed": True})

    def network_state(_sid: str) -> sandbox.WindowsCapabilityState:
        nonlocal network_calls
        network_calls += 1
        return ready if network_calls > 1 else sandbox.WindowsCapabilityState(
            "missing", True, "missing", {}
        )

    def powershell(command: str) -> subprocess.CompletedProcess[str]:
        if "New-NetFirewallRule" in command:
            calls.append(("firewall", "offline"))
        return subprocess.CompletedProcess(["powershell"], 0, "", "")

    monkeypatch.setattr(windows.os, "name", "nt", raising=False)
    monkeypatch.setattr(windows, "_is_elevated", lambda: True)
    monkeypatch.setattr(windows, "_legacy_artifact_diagnostics", lambda: ())
    monkeypatch.setattr(windows, "_ensure_sandbox_identity", ensure_identity)
    monkeypatch.setattr(windows, "_setup_identity_security", secure_identity)
    monkeypatch.setattr(
        windows,
        "_write_security_attestation",
        lambda _sids: windows._OperationResult(True, "verified", {"changed": True}),
    )
    monkeypatch.setattr(
        windows,
        "_ensure_state_dir_acl",
        lambda: windows._OperationResult(True, "ready", {"changed": True}),
    )
    monkeypatch.setattr(
        windows,
        "_account_sid",
        lambda name: "offline-sid" if name == windows.OFFLINE_SANDBOX_ACCOUNT else "online-sid",
    )
    monkeypatch.setattr(windows, "_network_state", network_state)
    monkeypatch.setattr(windows, "_online_network_filter_state", lambda _sid: ready)
    monkeypatch.setattr(windows, "_run_powershell", powershell)
    monkeypatch.setattr(windows, "_acl_state", lambda *_args: ready)
    monkeypatch.setattr(windows, "_runner_smoke_state", lambda *_args: ready)
    monkeypatch.setattr(windows, "_network_probe_state", lambda *_args: ready)
    monkeypatch.setattr(windows, "_has_windows_symbols", lambda *_args: True)
    monkeypatch.setattr(
        windows,
        "_cleanup_legacy_assets",
        lambda: windows._OperationResult(True, "removed", {"changed": False}),
    )
    monkeypatch.setattr(
        windows,
        "_hide_account_from_login_ui",
        lambda account: calls.append(("visibility", account))
        or windows._OperationResult(True, "hidden", {"changed": False}),
    )
    monkeypatch.setattr(windows, "_login_ui_visibility_state", lambda _account: ready)
    monkeypatch.setattr(windows.time, "sleep", lambda _seconds: None)
    monkeypatch.setattr(
        windows,
        "_probe_windows_sandbox_uncached",
        lambda: sandbox.WindowsSandboxDoctorReport.ready_for_tests(),
    )

    report = windows.setup_windows_sandbox()

    assert report.status == "ready"
    assert report.available_after_setup is True
    assert set(report.completed_steps) == set(windows.SETUP_STEP_ORDER)
    assert ("identity", "offline") in calls
    assert ("identity", "online") in calls
    assert ("security", "offline") in calls
    assert ("security", "online") in calls
    assert ("firewall", "offline") in calls
    assert ("visibility", windows.OFFLINE_SANDBOX_ACCOUNT) in calls
    assert ("visibility", windows.ONLINE_SANDBOX_ACCOUNT) in calls

def test_windows_setup_failed_probe_steps_include_structured_details(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    acl_evidence = {
        "operation": "acl_probe_root_mkdir",
        "state_dir_hash": "statehash",
        "probe_root_hash": "roothash",
        "errno": 13,
        "winerror": 5,
    }
    missing_acl = sandbox.WindowsCapabilityState(
        "missing",
        True,
        "ACL probe directory could not be created.",
        acl_evidence,
    )
    ready = sandbox.WindowsCapabilityState("available", True, "ready", {})

    monkeypatch.setattr(windows.os, "name", "nt", raising=False)
    monkeypatch.setattr(windows, "_is_elevated", lambda: True)
    monkeypatch.setattr(windows, "_legacy_artifact_diagnostics", lambda: ())
    monkeypatch.setattr(
        windows,
        "_ensure_sandbox_identity",
        lambda _identity: windows._OperationResult(True, "ready", {"changed": False}),
    )
    monkeypatch.setattr(
        windows,
        "_setup_identity_security",
        lambda _identity: windows._OperationResult(True, "ready", {"changed": False}),
    )
    monkeypatch.setattr(
        windows,
        "_write_security_attestation",
        lambda _sids: windows._OperationResult(True, "verified", {"changed": False}),
    )
    monkeypatch.setattr(
        windows,
        "_ensure_state_dir_acl",
        lambda: windows._OperationResult(True, "ready", {"changed": False}),
    )
    monkeypatch.setattr(windows, "_account_sid", lambda _name: "S-1-5-21-123")
    monkeypatch.setattr(windows, "_network_state", lambda _sid: ready)
    monkeypatch.setattr(windows, "_online_network_filter_state", lambda _sid: ready)
    monkeypatch.setattr(windows, "_acl_state", lambda *_args: missing_acl)
    monkeypatch.setattr(windows, "_runner_smoke_state", lambda *_args: ready)
    monkeypatch.setattr(windows, "_network_probe_state", lambda *_args: ready)
    monkeypatch.setattr(windows, "_has_windows_symbols", lambda *_args: True)
    monkeypatch.setattr(
        windows,
        "_probe_windows_sandbox_uncached",
        lambda: sandbox.WindowsSandboxDoctorReport.ready_for_tests(),
    )

    report = windows.setup_windows_sandbox()

    failure = next(item for item in report.failed_steps if item["step"] == "acl_boundary")
    assert failure["details"]["offline"]["evidence"] == acl_evidence
    assert failure["details"]["online"]["evidence"] == acl_evidence
    assert report.available_after_setup is False


def test_windows_backend_never_prepares_without_completed_setup(tmp_path: Path) -> None:
    ready = sandbox.WindowsSandboxDoctorReport.ready_for_tests()
    unavailable = replace(
        ready,
        available=False,
        enforcement_status="backend_unavailable",
        blocking_requirements=("setup:sandbox_accounts",),
    )
    backend = sandbox.WindowsSandboxBackend(doctor_provider=lambda: unavailable)

    with pytest.raises(SandboxCapabilityError, match="backend_unavailable"):
        backend.prepare(_request(tmp_path))


class _FakeReadyWindowsBackend(sandbox.WindowsSandboxBackend):
    def __init__(self, **kwargs) -> None:
        super().__init__(
            acl_applier=lambda _path, _account: None,
            run_root_provider=lambda request: (
                request.workspace_root / "work" / "sandboxes" / request.sandbox_id
            ),
            **kwargs,
        )

    def doctor(self) -> sandbox.WindowsSandboxDoctorReport:
        return sandbox.WindowsSandboxDoctorReport.ready_for_tests()

    def setup(self) -> sandbox.WindowsSandboxSetupReport:
        return sandbox.WindowsSandboxSetupReport.ready_for_tests()


@pytest.mark.security
@pytest.mark.parametrize(
    ("profile_name", "expected_account", "expected_role"),
    (
        (
            SandboxProfileName.ISOLATED_VERIFICATION,
            windows.OFFLINE_SANDBOX_ACCOUNT,
            "offline",
        ),
        (
            SandboxProfileName.PACKAGE_OPERATION,
            windows.ONLINE_SANDBOX_ACCOUNT,
            "online",
        ),
    ),
)
def test_windows_backend_routes_network_mode_to_dedicated_account(
    tmp_path: Path,
    profile_name: SandboxProfileName,
    expected_account: str,
    expected_role: str,
) -> None:
    backend = _FakeReadyWindowsBackend()
    request = _request(tmp_path)
    request.profile = sandbox.default_sandbox_profile(profile_name, workspace_root=tmp_path)

    prepared = backend.prepare(request)

    assert prepared.baseline["sandbox_account"] == expected_account
    assert prepared.baseline["credential_target"] == expected_account
    assert prepared.baseline["sandbox_role"] == expected_role


def test_windows_backend_uses_short_machine_state_run_root(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    state_dir = tmp_path / "machine-state"
    monkeypatch.setattr(windows, "_windows_state_dir_path", lambda: state_dir)
    backend = sandbox.WindowsSandboxBackend(
        acl_applier=lambda _path, _account: None,
        doctor_provider=sandbox.WindowsSandboxDoctorReport.ready_for_tests,
    )

    prepared = backend.prepare(_request(tmp_path))

    assert prepared.sandbox_root == state_dir / "runs" / "sandbox_windows"
    assert prepared.execution_cwd == prepared.sandbox_root / "workspace"


def test_windows_backend_rejects_unimplemented_network_modes_before_preparing(
    tmp_path: Path,
) -> None:
    backend = _FakeReadyWindowsBackend()
    request = _request(tmp_path)
    request.profile.network.mode = sandbox.SandboxNetworkMode.ALLOWLIST

    with pytest.raises(SandboxCapabilityError, match="network mode allowlist"):
        backend.prepare(request)


class _FakeRunner:
    def __init__(
        self,
        *,
        exit_code: int = 0,
        stdout: str = "ok\n",
        stderr: str = "",
        timed_out: bool = False,
        network_denied: bool = True,
        metadata: dict[str, object] | None = None,
    ) -> None:
        self.exit_code = exit_code
        self.stdout = stdout
        self.stderr = stderr
        self.timed_out = timed_out
        self.network_denied = network_denied
        self.metadata = metadata or {
            "runner": "fake",
            "backend": "windows",
            "restricted_token": True,
            "low_integrity": True,
            "private_desktop": True,
            "job_object": True,
        }
        self.calls: list[PreparedSandbox] = []

    def run(self, prepared: PreparedSandbox) -> sandbox.WindowsRunnerResult:
        self.calls.append(prepared)
        now = datetime.now(UTC).isoformat()
        return sandbox.WindowsRunnerResult(
            exit_code=None if self.timed_out else self.exit_code,
            stdout=self.stdout,
            stderr=self.stderr,
            timed_out=self.timed_out,
            started_at=now,
            ended_at=now,
            duration_ms=5,
            output_truncated=False,
            job_killed=self.timed_out,
            network_denied_verified=self.network_denied,
            metadata=self.metadata,
        )


def test_windows_backend_runs_low_risk_verification_with_ready_runner(tmp_path: Path) -> None:
    runner = _FakeRunner(stdout="pytest ok\n")
    backend = _FakeReadyWindowsBackend(runner=runner)
    request = _request(tmp_path)
    request.command = [sys.executable, "-m", "compileall", "."]

    prepared = backend.prepare(request)
    result = backend.run(prepared)

    assert runner.calls == [prepared]
    assert result.status == SandboxStatus.SUCCESS
    assert result.backend_name == "windows"
    assert result.exit_code == 0
    assert result.stdout == "pytest ok\n"
    assert result.metadata["execution_backend"] == "account_restricted_token"
    assert result.metadata["restricted_token"] is True
    assert result.metadata["low_integrity"] is True
    assert result.metadata["private_desktop"] is True
    assert result.metadata["process_tree_kill"] is True
    assert result.metadata["network_denied_verified"] is True


def test_windows_backend_does_not_forge_runner_enforcement_evidence(tmp_path: Path) -> None:
    backend = _FakeReadyWindowsBackend(
        runner=_FakeRunner(
            metadata={
                "runner": "fake",
                "backend": "windows",
                "restricted_token": False,
                "low_integrity": False,
                "private_desktop": False,
                "job_object": False,
            }
        )
    )
    prepared = backend.prepare(_request(tmp_path))

    result = backend.run(prepared)

    assert result.status == SandboxStatus.VIOLATION
    assert result.metadata["error_code"] == "sandbox_enforcement_failed"
    assert result.metadata["restricted_token"] is False
    assert result.metadata["low_integrity"] is False
    assert result.metadata["private_desktop"] is False
    assert result.metadata["process_tree_kill"] is False
    assert result.violations[0].violation_type == "process_isolation"


def test_windows_backend_timeout_maps_to_sandbox_timeout(tmp_path: Path) -> None:
    backend = _FakeReadyWindowsBackend(runner=_FakeRunner(timed_out=True))
    prepared = backend.prepare(_request(tmp_path))

    result = backend.run(prepared)

    assert result.status == SandboxStatus.TIMEOUT
    assert result.exit_code is None
    assert result.metadata["job_killed"] is True


def test_windows_backend_network_denied_self_test_fails_closed(tmp_path: Path) -> None:
    backend = _FakeReadyWindowsBackend(runner=_FakeRunner(network_denied=False))
    prepared = backend.prepare(_request(tmp_path))

    result = backend.run(prepared)

    assert result.status == SandboxStatus.VIOLATION
    assert result.exit_code is None
    assert result.metadata["error_code"] == "network_isolation_failed"
    assert result.violations[0].violation_type == "network"


def test_windows_backend_network_denied_requires_verified_firewall_probe(tmp_path: Path) -> None:
    class NoNetworkProofBackend(_FakeReadyWindowsBackend):
        def doctor(self) -> sandbox.WindowsSandboxDoctorReport:
            available = sandbox.WindowsSandboxDoctorReport.ready_for_tests()
            missing = sandbox.WindowsCapabilityState(
                "missing",
                True,
                "network probe missing",
                {"probe": "runtime"},
            )
            execution = sandbox.WindowsSandboxExecution(
                account_sids=available.execution.account_sids,
                credentials=available.execution.credentials,
                launchers=available.execution.launchers,
                runner_smoke=available.execution.runner_smoke,
                network_probe=missing,
            )
            return sandbox.WindowsSandboxDoctorReport(
                implementation=available.implementation,
                platform_supported=available.platform_supported,
                platform_status=available.platform_status,
                primitives=available.primitives,
                setup=available.setup,
                execution=execution,
                available=True,
                enforcement_status=available.enforcement_status,
                blocking_requirements=(),
                recommended_action=available.recommended_action,
            )

    backend = NoNetworkProofBackend(runner=_FakeRunner(network_denied=True))
    prepared = backend.prepare(_request(tmp_path))

    result = backend.run(prepared)

    assert result.status == SandboxStatus.VIOLATION
    assert result.metadata["error_code"] == "network_isolation_failed"
    assert result.metadata["network_filter_verified"] is True
    assert result.metadata["network_probe_verified"] is False
    assert result.metadata["network_denied_verified"] is False


def test_windows_backend_rechecks_enforcement_before_launching_runner(tmp_path: Path) -> None:
    class FlappingBackend(_FakeReadyWindowsBackend):
        def __init__(self, **kwargs) -> None:
            super().__init__(**kwargs)
            self.doctor_calls = 0

        def doctor(self) -> sandbox.WindowsSandboxDoctorReport:
            self.doctor_calls += 1
            if self.doctor_calls == 1:
                return sandbox.WindowsSandboxDoctorReport.ready_for_tests()
            available = sandbox.WindowsSandboxDoctorReport.ready_for_tests()
            return sandbox.WindowsSandboxDoctorReport(
                implementation=available.implementation,
                platform_supported=available.platform_supported,
                platform_status=available.platform_status,
                primitives=available.primitives,
                setup=available.setup,
                execution=available.execution,
                available=False,
                enforcement_status="backend_unavailable",
                blocking_requirements=("setup:network_filter",),
                recommended_action="rerun setup",
            )

    runner = _FakeRunner()
    backend = FlappingBackend(runner=runner)
    prepared = backend.prepare(_request(tmp_path))

    result = backend.run(prepared)

    assert result.status == SandboxStatus.BACKEND_UNAVAILABLE
    assert result.metadata["error_code"] == "backend_unavailable"
    assert runner.calls == []


def test_windows_backend_blocks_external_writable_paths_until_acl_projection_exists(
    tmp_path: Path,
) -> None:
    backend = _FakeReadyWindowsBackend()
    request = _request(tmp_path)
    request.profile.filesystem.writable_paths = [str(tmp_path), str(tmp_path.parent / "shared")]

    with pytest.raises(SandboxCapabilityError, match="additional writable directories"):
        backend.prepare(request)


def test_windows_backend_blocks_path_specific_readonly_until_acl_leases_exist(
    tmp_path: Path,
) -> None:
    backend = _FakeReadyWindowsBackend()
    request = _request(tmp_path)
    request.profile.filesystem.readonly_paths = ["src"]

    with pytest.raises(SandboxCapabilityError, match="readonly leases"):
        backend.prepare(request)


def test_windows_backend_redacts_output_and_respects_output_limit(tmp_path: Path) -> None:
    secret = "sk-test-secret-value"
    backend = _FakeReadyWindowsBackend(
        runner=_FakeRunner(stdout=f"{secret}\n" + ("A" * 100), stderr="")
    )
    request = _request(tmp_path)
    request.profile.resources.max_output_chars = 40
    prepared = backend.prepare(request)

    result = backend.run(prepared)

    assert secret not in result.stdout
    assert "redacted" in result.stdout.lower()
    assert len(result.stdout) <= 40
    assert result.metadata["output_truncated"] is True


def test_apply_account_acl_scopes_low_integrity_to_workspace(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    commands: list[list[str]] = []

    def fake_run_command(command: list[str]) -> subprocess.CompletedProcess[str]:
        commands.append(command)
        return subprocess.CompletedProcess(command, 0, "", "")

    monkeypatch.setattr(windows.shutil, "which", lambda _name: "icacls.exe")
    monkeypatch.setattr(windows, "_run_command", fake_run_command)
    run_root = tmp_path / "run"
    workspace_root = run_root / "workspace"
    workspace_root.mkdir(parents=True)

    result = windows._apply_account_acl(run_root, low_integrity_root=workspace_root)

    assert result.ok
    assert commands[0][1] == str(run_root)
    assert commands[1][1] == str(workspace_root)


def test_run_root_acl_removes_inherited_sandbox_accounts_and_preserves_host_cleanup(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    commands: list[list[str]] = []
    monkeypatch.setattr(windows.os, "name", "nt", raising=False)
    monkeypatch.setattr(windows, "_current_process_sid", lambda: "S-1-5-21-host")
    monkeypatch.setattr(windows.shutil, "which", lambda _name: "icacls.exe")
    monkeypatch.setattr(
        windows,
        "_run_command",
        lambda command: commands.append(command)
        or subprocess.CompletedProcess(command, 0, "", ""),
    )
    run_root = tmp_path / "run"
    (run_root / "workspace").mkdir(parents=True)
    backend = sandbox.WindowsSandboxBackend(
        doctor_provider=sandbox.WindowsSandboxDoctorReport.ready_for_tests,
    )

    backend._apply_run_acl(run_root, windows.OFFLINE_SANDBOX_ACCOUNT)

    flattened = [" ".join(command) for command in commands]
    assert "/inheritance:r" in flattened[0]
    grant = next(command for command in flattened if "S-1-5-21-host" in command)
    assert windows.OFFLINE_SANDBOX_ACCOUNT in grant
    assert windows.ONLINE_SANDBOX_ACCOUNT not in grant
    assert "(OI)(CI)F" in grant


def test_network_probe_requires_host_connectivity_baseline(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    launched = False

    def fake_run(_self, _prepared):
        nonlocal launched
        launched = True
        raise AssertionError("runner should not launch without host network baseline")

    monkeypatch.setattr(
        windows,
        "_network_state",
        lambda _sid: sandbox.WindowsCapabilityState(
            "available",
            True,
            "firewall configured",
            {},
        ),
    )
    monkeypatch.setattr(
        windows,
        "_host_network_baseline_state",
        lambda: sandbox.WindowsCapabilityState(
            "missing",
            True,
            "Host outbound connectivity baseline failed.",
            {"probe": "host"},
        ),
    )
    monkeypatch.setattr(windows.WindowsSandboxRunner, "run", fake_run)

    state = windows._network_probe_state(
        windows._sandbox_identity_for_mode(sandbox.SandboxNetworkMode.DENIED),
        "S-1-5-21-123",
    )

    assert state.status == "missing"
    assert "baseline" in state.reason.lower()
    assert launched is False


def test_online_network_probe_requires_real_connectivity_from_online_account(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    ready = sandbox.WindowsCapabilityState("available", True, "ready", {})
    constructor_args: list[dict[str, str]] = []

    class OnlineRunner:
        def __init__(self, **kwargs: str) -> None:
            constructor_args.append(kwargs)

        def run(self, _prepared) -> sandbox.WindowsRunnerResult:
            return sandbox.WindowsRunnerResult(
                exit_code=0,
                stdout="network-allowed\n",
                stderr="",
                timed_out=False,
                started_at="2026-06-28T00:00:00+00:00",
                ended_at="2026-06-28T00:00:01+00:00",
                duration_ms=1,
                network_denied_verified=True,
                metadata={"account_sid_hash": "hash"},
            )

    monkeypatch.setattr(windows.os, "name", "nt", raising=False)
    monkeypatch.setattr(windows, "_host_network_baseline_state", lambda: ready)
    monkeypatch.setattr(windows, "_windows_state_dir", lambda: tmp_path)
    monkeypatch.setattr(
        windows,
        "_apply_account_acl",
        lambda _path, **_kwargs: windows._OperationResult(True),
    )
    monkeypatch.setattr(windows, "WindowsSandboxRunner", OnlineRunner)
    identity = windows._sandbox_identity_for_mode(sandbox.SandboxNetworkMode.ALLOWED)

    state = windows._network_probe_state(identity, "S-1-5-21-456")

    assert state.status == "available"
    assert constructor_args == [
        {
            "account_name": windows.ONLINE_SANDBOX_ACCOUNT,
            "credential_target": windows.ONLINE_SANDBOX_ACCOUNT,
        }
    ]


def test_acl_probe_mkdir_oserror_reports_structured_diagnostics(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    probe_root = Path("C:/Users/Lenovo/AppData/Local/Singularity/windows-sandbox/acl-probe")

    monkeypatch.setattr(windows.os, "name", "nt", raising=False)
    monkeypatch.setattr(windows, "_account_sid", lambda _name: "S-1-5-21-123")
    monkeypatch.setattr(
        windows,
        "_credential_state",
        lambda *_args: sandbox.WindowsCapabilityState("available", True, "credential", {}),
    )
    monkeypatch.setattr(windows, "_windows_state_dir", lambda: probe_root.parent)

    def fail_mkdir(self: Path, *args, **kwargs) -> None:
        if self == probe_root:
            exc = PermissionError(13, "Access is denied")
            exc.winerror = 5
            raise exc
        return None

    monkeypatch.setattr(Path, "mkdir", fail_mkdir)

    state = windows._acl_state(
        True,
        windows._sandbox_identity_for_mode(sandbox.SandboxNetworkMode.DENIED),
    )

    assert state.status == "missing"
    assert "ACL probe directory could not be created" in state.reason
    evidence = state.evidence
    assert evidence["operation"] == "acl_probe_root_mkdir"
    assert evidence["errno"] == 13
    assert evidence["winerror"] == 5
    assert evidence["strerror"] == "Access is denied"
    assert evidence["elevated"] in {True, False}
    assert evidence["state_dir_hash"]
    assert evidence["probe_root_hash"]
    assert "C:/Users" not in json.dumps(evidence)
    assert "S-1-5-21" not in json.dumps(evidence)


def test_runner_smoke_oserror_reports_structured_diagnostics(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    probe_root = Path("C:/ProgramData/Singularity/windows-sandbox/runner-smoke")

    monkeypatch.setattr(windows.os, "name", "nt", raising=False)
    monkeypatch.setattr(
        windows,
        "_runner_state",
        lambda: sandbox.WindowsCapabilityState("available", True, "runner", {}),
    )
    monkeypatch.setattr(
        windows,
        "_credential_state",
        lambda *_args: sandbox.WindowsCapabilityState("available", True, "credential", {}),
    )
    monkeypatch.setattr(windows, "_account_sid", lambda _name: "S-1-5-21-123")
    monkeypatch.setattr(windows, "_windows_state_dir", lambda: probe_root.parent)
    monkeypatch.setattr(
        windows,
        "_apply_account_acl",
        lambda _path, **_kwargs: windows._OperationResult(True),
    )

    class FailingRunner:
        def run(self, _prepared):
            exc = OSError(22, "Invalid argument")
            exc.winerror = 87
            raise exc

    monkeypatch.setattr(windows, "WindowsSandboxRunner", lambda **_kwargs: FailingRunner())

    state = windows._runner_smoke_state(
        windows._sandbox_identity_for_mode(sandbox.SandboxNetworkMode.DENIED)
    )

    assert state.status == "missing"
    evidence = state.evidence
    assert evidence["operation"] == "runner_smoke_launch"
    assert evidence["errno"] == 22
    assert evidence["winerror"] == 87
    assert evidence["state_dir_hash"]
    assert evidence["probe_root_hash"]
    assert "C:/ProgramData" not in json.dumps(evidence)


def test_network_probe_oserror_reports_structured_diagnostics(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    probe_root = Path("C:/ProgramData/Singularity/windows-sandbox/network-smoke")

    monkeypatch.setattr(windows.os, "name", "nt", raising=False)
    monkeypatch.setattr(
        windows,
        "_network_state",
        lambda _sid: sandbox.WindowsCapabilityState("available", True, "firewall", {}),
    )
    monkeypatch.setattr(
        windows,
        "_host_network_baseline_state",
        lambda: sandbox.WindowsCapabilityState("available", True, "host baseline", {}),
    )
    monkeypatch.setattr(windows, "_windows_state_dir", lambda: probe_root.parent)
    monkeypatch.setattr(
        windows,
        "_apply_account_acl",
        lambda _path, **_kwargs: windows._OperationResult(True),
    )

    class FailingRunner:
        def run(self, _prepared):
            exc = OSError(5, "Access is denied")
            exc.winerror = 5
            raise exc

    monkeypatch.setattr(windows, "WindowsSandboxRunner", lambda **_kwargs: FailingRunner())

    state = windows._network_probe_state(
        windows._sandbox_identity_for_mode(sandbox.SandboxNetworkMode.DENIED),
        "S-1-5-21-123",
    )

    assert state.status == "missing"
    evidence = state.evidence
    assert evidence["operation"] == "network_probe_runner_launch"
    assert evidence["errno"] == 5
    assert evidence["winerror"] == 5
    assert evidence["probe_root_hash"]
    assert "S-1-5-21" not in json.dumps(evidence)


def test_windows_setup_state_dir_unwritable_fails_closed_with_hash_diagnostics(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    state_root = Path("C:/ProgramData/Singularity/windows-sandbox")

    monkeypatch.setattr(windows.os, "name", "nt", raising=False)
    monkeypatch.setattr(windows.os.environ, "get", lambda name, default=None: "C:\\ProgramData" if name == "PROGRAMDATA" else default)

    def fail_mkdir(self: Path, *args, **kwargs) -> None:
        if self == state_root:
            exc = PermissionError(13, "Access is denied")
            exc.winerror = 5
            raise exc
        return None

    monkeypatch.setattr(Path, "mkdir", fail_mkdir)

    result = windows._ensure_state_dir_acl()

    assert result.ok is False
    assert "could not be created" in result.reason
    assert result.details["operation"] == "state_dir_acl_mkdir"
    assert result.details["state_dir_hash"]
    assert result.details["winerror"] == 5
    assert "C:/ProgramData" not in json.dumps(result.details)


def test_windows_state_dir_probe_does_not_recreate_missing_cleanup_target(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    state_dir = tmp_path / "Singularity" / "windows-sandbox"
    mkdir_calls: list[Path] = []

    monkeypatch.setattr(windows.os, "name", "nt", raising=False)
    monkeypatch.setattr(windows, "_windows_state_dir_path", lambda: state_dir)

    original_mkdir = Path.mkdir

    def record_mkdir(self: Path, *args, **kwargs) -> None:
        mkdir_calls.append(self)
        original_mkdir(self, *args, **kwargs)

    monkeypatch.setattr(Path, "mkdir", record_mkdir)

    state = windows._state_dir_state()

    assert state.status == "missing"
    assert state_dir.exists() is False
    assert mkdir_calls == []


def test_probe_completed_process_diagnostics_sanitizes_paths() -> None:
    probe_root = Path("C:/Users/Lenovo/AppData/Local/Singularity/windows-sandbox/acl-probe")
    result = subprocess.CompletedProcess(
        ["icacls", str(probe_root)],
        5,
        f"processed file: {probe_root}",
        f"{probe_root}: Access is denied",
    )

    details = windows._completed_process_diagnostics(
        "acl_probe_deny_icacls",
        result,
        state_dir=probe_root.parent,
        probe_root=probe_root,
        path=probe_root / "denied",
    )

    encoded = json.dumps(details)
    assert "C:/Users" not in encoded
    assert "AppData" not in encoded
    assert "<path:" in details["stdout_summary"]
    assert "<path:" in details["stderr_summary"]
    assert details["path_hash"]


def test_probe_completed_process_diagnostics_preserves_wrapped_subprocess_oserror(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    probe_root = Path("C:/Users/Lenovo/AppData/Local/Singularity/windows-sandbox/acl-probe")

    def fail_run(*args, **kwargs):
        exc = PermissionError(13, f"Access denied: {probe_root}")
        exc.winerror = 5
        raise exc

    monkeypatch.setattr(windows.subprocess, "run", fail_run)

    completed = windows._run_command(["icacls", str(probe_root)])
    details = windows._completed_process_diagnostics(
        "acl_probe_deny_icacls",
        completed,
        state_dir=probe_root.parent,
        probe_root=probe_root,
        path=probe_root / "denied",
    )

    encoded = json.dumps(details)
    assert details["returncode"] == 1
    assert details["errno"] == 13
    assert details["winerror"] == 5
    assert details["error_type"] == "PermissionError"
    assert details["subprocess_operation"] == "subprocess"
    assert "C:/Users" not in encoded
    assert "AppData" not in encoded


def test_sandbox_setup_cli_reports_structured_exception_diagnostics(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    def fail_setup(self):
        exc = OSError(22, "Invalid argument: C:/Users/Lenovo/AppData/Local/Singularity")
        exc.winerror = 87
        raise exc

    monkeypatch.setattr(sandbox.WindowsSandboxBackend, "setup", fail_setup)

    result = CliRunner().invoke(app, ["sandbox", "setup", "--json"])

    assert result.exit_code == 1
    payload = json.loads(result.stdout)
    assert payload["status"] == "backend_unavailable"
    assert payload["message"] == "Windows sandbox setup failed; inspect diagnostics for operation details."
    assert "C:/Users" not in payload["message"]
    assert payload["diagnostics"][0]["operation"] == "sandbox_setup"
    assert payload["diagnostics"][0]["errno"] == 22
    assert payload["diagnostics"][0]["winerror"] == 87


def test_windows_runner_result_serialization_redacts_output() -> None:
    secret = "sk-test-secret-value"
    result = sandbox.WindowsRunnerResult(
        exit_code=0,
        stdout=f"{secret}\n",
        stderr=f"token={secret}\n",
        timed_out=False,
        started_at="2026-06-27T00:00:00+00:00",
        ended_at="2026-06-27T00:00:01+00:00",
        duration_ms=1,
        metadata={"api_key": secret},
    )

    payload = result.to_dict()

    encoded = json.dumps(payload, sort_keys=True)
    assert secret not in encoded
    assert "<redacted>" in encoded


def test_child_output_removes_raw_temp_files(tmp_path: Path) -> None:
    stdout_path = tmp_path / "child.stdout"
    stderr_path = tmp_path / "child.stderr"
    stdout_path.write_text("stdout", encoding="utf-8")
    stderr_path.write_text("stderr", encoding="utf-8")
    process = sandbox.windows_runner._WindowsChildProcess(
        command=["python"],
        process_handle=0,
        thread_handle=0,
        process_id=123,
        job_handle=None,
        job_assigned=False,
        desktop_handle=None,
        stdout_path=stdout_path,
        stderr_path=stderr_path,
        streams=[],
    )

    stdout, stderr, _truncated = sandbox.windows_runner._child_output(process, None)

    assert stdout == "stdout"
    assert stderr == "stderr"
    assert not stdout_path.exists()
    assert not stderr_path.exists()


def test_windows_sandbox_runner_removes_account_runner_logs(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    run_root = tmp_path / "run"
    run_root.mkdir()
    result_path = run_root / "runner-result.json"
    result = sandbox.WindowsRunnerResult(
        exit_code=0,
        stdout="ok",
        stderr="",
        timed_out=False,
        started_at="2026-06-27T00:00:00+00:00",
        ended_at="2026-06-27T00:00:01+00:00",
        duration_ms=1,
    )
    result_path.write_text(json.dumps(result.to_dict()), encoding="utf-8")
    stdout_path = run_root / "account-runner.stdout"
    stderr_path = run_root / "account-runner.stderr"
    stdout_path.write_text("sk-test-secret-value", encoding="utf-8")
    stderr_path.write_text("stderr", encoding="utf-8")
    class FakeAccountProcess:
        def __init__(self) -> None:
            self.args = ["python"]
            self.returncode = 0

        def wait(self, timeout=None):
            return 0

        def kill(self):
            self.returncode = 1

        def stdout_text(self):
            return sandbox.windows_runner._read_and_unlink_text(stdout_path)

        def stderr_text(self):
            return sandbox.windows_runner._read_and_unlink_text(stderr_path)

    fake_process = FakeAccountProcess()
    prepared = type(
        "Prepared",
        (),
        {
            "baseline": {"runner_spec": str(run_root / "runner-spec.json"), "runner_result": str(result_path)},
            "sandbox_root": run_root,
            "request": type(
                "Request",
                (),
                {"profile": type("Profile", (), {"resources": type("Resources", (), {"timeout_seconds": 1})()})()},
            )(),
        },
    )()

    monkeypatch.setattr(sandbox.windows_runner.os, "name", "nt")
    monkeypatch.setattr(
        sandbox.windows_runner,
        "_read_generic_credential",
        lambda _target: (windows.OFFLINE_SANDBOX_ACCOUNT, "secret"),
    )
    monkeypatch.setattr(
        sandbox.windows_runner,
        "_start_account_process",
        lambda *args, **kwargs: fake_process,
    )

    runner_result = sandbox.WindowsSandboxRunner().run(prepared)

    assert runner_result.exit_code == 0
    assert not stdout_path.exists()
    assert not stderr_path.exists()


def test_account_logon_rights_view_classifies_rights() -> None:
    empty = windows._logon_rights_view([], "")
    assert empty["interactive"] is False
    assert empty["deny_interactive"] is False
    assert empty["rights"] == []

    granted = windows._logon_rights_view(
        ["SeInteractiveLogonRight", "SeBatchLogonRight"], ""
    )
    assert granted["interactive"] is True
    assert granted["batch"] is True
    assert granted["deny_interactive"] is False

    denied = windows._logon_rights_view(
        ["SeInteractiveLogonRight", "SeDenyInteractiveLogonRight", "SeDenyBatchLogonRight"],
        "",
    )
    assert denied["interactive"] is True
    assert denied["deny_interactive"] is True
    assert denied["deny_batch"] is True


def test_windows_doctor_launcher_reports_logon_rights_and_blocks_when_right_missing(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(windows.os, "name", "nt", raising=False)
    monkeypatch.setattr(windows, "_has_windows_symbols", lambda *_args: True)
    monkeypatch.setattr(windows, "_executable_acl_summary", lambda: "D:\\anconda3\\python.exe BUILTIN\\Users:(I)(RX)")
    monkeypatch.setattr(windows.sys, "executable", "D:\\anconda3\\python.exe")

    rights = windows._logon_rights_view([], "")
    state = windows._launcher_state(
        windows._sandbox_identity_for_mode(sandbox.SandboxNetworkMode.DENIED),
        "S-1-5-21-123",
        rights,
        acl_boundary_ready=True,
    )

    assert state.status == "missing"
    evidence = state.evidence
    assert evidence["api"] == "CreateProcessWithLogonW"
    assert evidence["logon_flags"] == "0 (profile not loaded)"
    assert evidence["domain_username_form"].startswith(".\\")
    assert evidence["symbol_present"] is True
    assert evidence["account_logon_rights"]["interactive"] is False
    assert evidence["account_logon_rights"]["deny_interactive"] is False
    assert evidence["window_station"]["inherits_parent"] is True
    assert evidence["window_station"]["lpDesktop"] is None
    assert evidence["desktop"]["inherits_parent"] is True
    assert evidence["executable"]["path_hash"]
    assert "D:\\anconda3\\python.exe" not in evidence["executable"]["acl_summary_redacted"]
    assert evidence["working_directory"]["account_has_access"] is True
    assert "S-1-5-21" not in json.dumps(state.to_dict())


def test_windows_doctor_launcher_available_when_interactive_right_present(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(windows.os, "name", "nt", raising=False)
    monkeypatch.setattr(windows, "_has_windows_symbols", lambda *_args: True)
    monkeypatch.setattr(windows, "_executable_acl_summary", lambda: "")

    rights = windows._logon_rights_view(["SeInteractiveLogonRight"], "")
    state = windows._launcher_state(
        windows._sandbox_identity_for_mode(sandbox.SandboxNetworkMode.DENIED),
        "S-1-5-21-123",
        rights,
        acl_boundary_ready=False,
    )

    assert state.status == "available"
    assert state.evidence["account_logon_rights"]["interactive"] is True


def test_windows_doctor_launcher_blocks_when_deny_interactive_right_present(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(windows.os, "name", "nt", raising=False)
    monkeypatch.setattr(windows, "_has_windows_symbols", lambda *_args: True)
    monkeypatch.setattr(windows, "_executable_acl_summary", lambda: "")

    rights = windows._logon_rights_view(
        ["SeInteractiveLogonRight", "SeDenyInteractiveLogonRight"], ""
    )
    state = windows._launcher_state(
        windows._sandbox_identity_for_mode(sandbox.SandboxNetworkMode.DENIED),
        "S-1-5-21-123",
        rights,
        acl_boundary_ready=True,
    )

    assert state.status == "missing"
    assert state.evidence["account_logon_rights"]["deny_interactive"] is True


def test_windows_doctor_launcher_defers_when_rights_unverifiable_non_elevated(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # After an elevated setup grants SeInteractiveLogonRight, a non-elevated
    # LsaEnumerateAccountRights may return STATUS_ACCESS_DENIED (0xC0000022)
    # for an account that DOES hold rights. launcher must defer to runner_smoke
    # rather than falsely block the backend after the right was granted.
    monkeypatch.setattr(windows.os, "name", "nt", raising=False)
    monkeypatch.setattr(windows, "_has_windows_symbols", lambda *_args: True)
    monkeypatch.setattr(windows, "_executable_acl_summary", lambda: "")

    rights = windows._logon_rights_view([], "0xC0000022")
    state = windows._launcher_state(
        windows._sandbox_identity_for_mode(sandbox.SandboxNetworkMode.DENIED),
        "S-1-5-21-123",
        rights,
        acl_boundary_ready=False,
    )

    assert state.status == "available"
    assert state.evidence["account_logon_rights"]["lsa_status"] == "0xC0000022"


def test_runner_smoke_blocks_when_account_identity_mismatches(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    monkeypatch.setattr(windows.os, "name", "nt", raising=False)
    monkeypatch.setattr(
        windows,
        "_runner_state",
        lambda: sandbox.WindowsCapabilityState("available", True, "runner", {}),
    )
    monkeypatch.setattr(
        windows,
        "_credential_state",
        lambda *_args: sandbox.WindowsCapabilityState("available", True, "credential", {}),
    )
    monkeypatch.setattr(windows, "_account_sid", lambda _name: "S-1-5-21-123")
    monkeypatch.setattr(windows, "_windows_state_dir", lambda: tmp_path)
    monkeypatch.setattr(
        windows,
        "_apply_account_acl",
        lambda _path, **_kwargs: windows._OperationResult(True),
    )

    class MismatchedRunner:
        def run(self, _prepared):
            return sandbox.WindowsRunnerResult(
                exit_code=0,
                stdout="sandbox-smoke\n",
                stderr="",
                timed_out=False,
                started_at="2026-06-28T00:00:00+00:00",
                ended_at="2026-06-28T00:00:01+00:00",
                duration_ms=1,
                metadata={
                    "restricted_token": True,
                    "low_integrity": True,
                    "private_desktop": True,
                    "job_object": True,
                    "account_sid_hash": "mismatch",
                    "account_name": "OtherUser",
                },
            )

    monkeypatch.setattr(windows, "WindowsSandboxRunner", lambda **_kwargs: MismatchedRunner())

    state = windows._runner_smoke_state(
        windows._sandbox_identity_for_mode(sandbox.SandboxNetworkMode.DENIED)
    )

    assert state.status == "missing"
    assert state.evidence["account_identity_verified"] is False
    assert state.evidence["account_sid_hash"] == "mismatch"


def test_runner_smoke_passes_when_account_identity_matches(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    monkeypatch.setattr(windows.os, "name", "nt", raising=False)
    monkeypatch.setattr(
        windows,
        "_runner_state",
        lambda: sandbox.WindowsCapabilityState("available", True, "runner", {}),
    )
    monkeypatch.setattr(
        windows,
        "_credential_state",
        lambda *_args: sandbox.WindowsCapabilityState("available", True, "credential", {}),
    )
    expected_hash = windows._hash_sid("S-1-5-21-123")
    monkeypatch.setattr(windows, "_account_sid", lambda _name: "S-1-5-21-123")
    monkeypatch.setattr(windows, "_windows_state_dir", lambda: tmp_path)
    monkeypatch.setattr(
        windows,
        "_apply_account_acl",
        lambda _path, **_kwargs: windows._OperationResult(True),
    )

    class MatchingRunner:
        def run(self, _prepared):
            return sandbox.WindowsRunnerResult(
                exit_code=0,
                stdout="sandbox-smoke\n",
                stderr="",
                timed_out=False,
                started_at="2026-06-28T00:00:00+00:00",
                ended_at="2026-06-28T00:00:01+00:00",
                duration_ms=1,
                metadata={
                    "restricted_token": True,
                    "low_integrity": True,
                    "private_desktop": True,
                    "job_object": True,
                    "account_sid_hash": expected_hash,
                    "account_name": windows.OFFLINE_SANDBOX_ACCOUNT,
                },
            )

    monkeypatch.setattr(windows, "WindowsSandboxRunner", lambda **_kwargs: MatchingRunner())

    state = windows._runner_smoke_state(
        windows._sandbox_identity_for_mode(sandbox.SandboxNetworkMode.DENIED)
    )

    assert state.status == "available"
    assert state.evidence["account_identity_verified"] is True


def _hardened_logon_rights() -> dict[str, object]:
    return windows._logon_rights_view(
        [
            "SeInteractiveLogonRight",
            "SeDenyBatchLogonRight",
            "SeDenyNetworkLogonRight",
            "SeDenyRemoteInteractiveLogonRight",
            "SeDenyServiceLogonRight",
        ],
        "",
    )


def _patch_identity_security_common(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(windows.os, "name", "nt", raising=False)
    monkeypatch.setattr(windows, "_account_sid", lambda _name: "S-1-5-21-123")
    monkeypatch.setattr(
        windows,
        "_hide_account_from_login_ui",
        lambda _name: windows._OperationResult(True, "already_hidden", {"changed": False}),
    )
    monkeypatch.setattr(
        windows,
        "_remove_deny_logon_rights",
        lambda _sid: windows._OperationResult(True, "none", {"changed": False}),
    )
    monkeypatch.setattr(
        windows,
        "_harden_sandbox_logon_rights",
        lambda _sid: windows._OperationResult(True, "hardened", {"changed": True}),
    )
    monkeypatch.setattr(
        windows,
        "_ensure_constrained_group_membership",
        lambda _name: windows._OperationResult(True, "constrained", {"changed": True}),
    )


def test_setup_identity_security_grants_and_verifies_interactive_logon(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _patch_identity_security_common(monkeypatch)
    states = [windows._logon_rights_view([], ""), _hardened_logon_rights()]
    grant_calls: list[str] = []
    monkeypatch.setattr(windows, "_enumerate_account_logon_rights", lambda _sid: states.pop(0))
    monkeypatch.setattr(
        windows,
        "_grant_logon_right",
        lambda sid: grant_calls.append(sid) or windows._OperationResult(True),
    )

    result = windows._setup_identity_security(
        windows._sandbox_identity_for_mode(sandbox.SandboxNetworkMode.DENIED)
    )

    assert result.ok is True
    assert grant_calls == ["S-1-5-21-123"]
    assert result.details["changed"] is True


def test_setup_identity_security_fails_when_interactive_grant_fails(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _patch_identity_security_common(monkeypatch)
    monkeypatch.setattr(
        windows,
        "_enumerate_account_logon_rights",
        lambda _sid: windows._logon_rights_view([], ""),
    )
    monkeypatch.setattr(
        windows,
        "_grant_logon_right",
        lambda _sid: windows._OperationResult(False, "LsaAddAccountRights failed"),
    )

    result = windows._setup_identity_security(
        windows._sandbox_identity_for_mode(sandbox.SandboxNetworkMode.DENIED)
    )

    assert result.ok is False
    assert "LsaAddAccountRights failed" in result.reason


def test_setup_identity_security_fails_when_post_verify_is_incomplete(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _patch_identity_security_common(monkeypatch)
    incomplete = windows._logon_rights_view(["SeInteractiveLogonRight"], "")
    monkeypatch.setattr(windows, "_enumerate_account_logon_rights", lambda _sid: incomplete)
    monkeypatch.setattr(
        windows,
        "_grant_logon_right",
        lambda _sid: windows._OperationResult(True),
    )

    result = windows._setup_identity_security(
        windows._sandbox_identity_for_mode(sandbox.SandboxNetworkMode.DENIED)
    )

    assert result.ok is False
    assert "not verified" in result.reason


def test_setup_identity_security_records_group_hardening_failure(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _patch_identity_security_common(monkeypatch)
    monkeypatch.setattr(
        windows,
        "_enumerate_account_logon_rights",
        lambda _sid: _hardened_logon_rights(),
    )
    monkeypatch.setattr(
        windows,
        "_grant_logon_right",
        lambda _sid: windows._OperationResult(True),
    )
    monkeypatch.setattr(
        windows,
        "_ensure_constrained_group_membership",
        lambda _name: windows._OperationResult(
            False,
            "Failed to constrain sandbox account local group membership.",
            {"windows_error_code": 5},
        ),
    )

    result = windows._setup_identity_security(
        windows._sandbox_identity_for_mode(sandbox.SandboxNetworkMode.ALLOWED)
    )

    assert result.ok is False
    assert "group membership" in result.reason
    assert result.details["windows_error_code"] == 5


def test_windows_child_process_close_handles_is_idempotent() -> None:
    import singularity.sandbox.windows_runner as runner

    child = runner._WindowsChildProcess(
        command=["python"],
        process_handle=0,
        thread_handle=0,
        process_id=0,
        job_handle=None,
        job_assigned=False,
        desktop_handle=None,
        stdout_path=Path("stdout"),
        stderr_path=Path("stderr"),
        streams=[],
    )

    child._close_handles()
    # Second call must be a no-op (the _closed guard short-circuits).
    child._close_handles()
    assert child._closed is True


def test_windows_extended_path_supports_long_local_and_unc_working_directories() -> None:
    import singularity.sandbox.windows_runner as runner

    assert runner._windows_extended_path(Path(r"C:\\work\\sandbox")) == (
        r"\\?\C:\work\sandbox"
    )
    assert runner._windows_extended_path(Path(r"\\server\share\sandbox")) == (
        r"\\?\UNC\server\share\sandbox"
    )
    assert runner._windows_extended_path(Path(r"\\?\C:\work\sandbox")) == (
        r"\\?\C:\work\sandbox"
    )
