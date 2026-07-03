from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

from singularity.observability.redaction import TraceRedactor
from singularity.sandbox.exceptions import SandboxCapabilityError
from singularity.sandbox.models import SandboxNetworkMode

DOCTOR_SCHEMA_VERSION = "sandbox.windows.doctor/v2"
SETUP_SCHEMA_VERSION = "sandbox.windows.setup/v2"
CLEANUP_SCHEMA_VERSION = "sandbox.windows.cleanup/v2"
WINDOWS_LOCAL_ACCOUNT_NAME_LIMIT = 20
OFFLINE_SANDBOX_ACCOUNT = "SingularityOffline"
ONLINE_SANDBOX_ACCOUNT = "SingularityOnline"
SANDBOX_ACCOUNTS = (OFFLINE_SANDBOX_ACCOUNT, ONLINE_SANDBOX_ACCOUNT)
LEGACY_SINGLE_SANDBOX_ACCOUNT = "SingularitySandbox"
LEGACY_SANDBOX_ACCOUNT = "SingularitySandboxRunner"
LEGACY_SANDBOX_ACCOUNTS = (
    LEGACY_SINGLE_SANDBOX_ACCOUNT,
    LEGACY_SANDBOX_ACCOUNT,
)
FIREWALL_RULE_GROUP = "Singularity Sandbox"
FIREWALL_RULE_NAME = "Singularity Sandbox Outbound Block"
LEGACY_FIREWALL_RULE_NAME = "Singularity Sandbox Runner Outbound Block"
READINESS_SNAPSHOT_TTL_SECONDS = 30.0
LOGIN_UI_USERLIST_KEY = (
    r"HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon\SpecialAccounts\UserList"
)
SECURITY_ATTESTATION_KEY = r"HKLM:\SOFTWARE\Singularity\WindowsSandbox"
SECURITY_ATTESTATION_VALUE = "SecurityAttestation"
SECURITY_ATTESTATION_SCHEMA_VERSION = "singularity.windows-sandbox.security/v1"
SECURITY_ATTESTATION_POLICY = "interactive-only-with-remote-network-service-batch-denied"
SETUP_STEP_ORDER = (
    "sandbox_accounts",
    "credentials",
    "login_ui_visibility",
    "logon_rights",
    "group_membership",
    "state_dir_acl",
    "acl_boundary",
    "offline_network_filter",
    "private_desktop",
    "execution_backends",
    "network_probe",
    "legacy_cleanup",
)


@dataclass(frozen=True)
class _WindowsSandboxIdentity:
    role: str
    network_mode: SandboxNetworkMode
    account_name: str
    credential_target: str
    firewall_blocked: bool


_SANDBOX_IDENTITIES = {
    SandboxNetworkMode.DENIED: _WindowsSandboxIdentity(
        role="offline",
        network_mode=SandboxNetworkMode.DENIED,
        account_name=OFFLINE_SANDBOX_ACCOUNT,
        credential_target=OFFLINE_SANDBOX_ACCOUNT,
        firewall_blocked=True,
    ),
    SandboxNetworkMode.ALLOWED: _WindowsSandboxIdentity(
        role="online",
        network_mode=SandboxNetworkMode.ALLOWED,
        account_name=ONLINE_SANDBOX_ACCOUNT,
        credential_target=ONLINE_SANDBOX_ACCOUNT,
        firewall_blocked=False,
    ),
}


def _sandbox_identity_for_mode(mode: SandboxNetworkMode) -> _WindowsSandboxIdentity:
    identity = _SANDBOX_IDENTITIES.get(mode)
    if identity is None:
        raise SandboxCapabilityError(
            f"backend_unavailable: Windows sandbox network mode {mode.value} is unsupported."
        )
    return identity


@dataclass(frozen=True)
class WindowsCapabilityState:
    status: str
    checked: bool
    reason: str
    evidence: dict[str, Any] = field(default_factory=dict)

    @property
    def ready(self) -> bool:
        return self.status == "available"

    def to_dict(self) -> dict[str, Any]:
        redacted = TraceRedactor().redact_value(self.evidence)
        return {
            "status": self.status,
            "checked": self.checked,
            "reason": self.reason,
            "evidence": redacted if isinstance(redacted, dict) else {},
        }


@dataclass(frozen=True)
class WindowsSandboxPrimitives:
    restricted_token: WindowsCapabilityState
    job_object: WindowsCapabilityState
    low_integrity: WindowsCapabilityState
    acl: WindowsCapabilityState
    firewall: WindowsCapabilityState
    private_desktop: WindowsCapabilityState

    def values(self) -> tuple[WindowsCapabilityState, ...]:
        return (
            self.restricted_token,
            self.job_object,
            self.low_integrity,
            self.acl,
            self.firewall,
            self.private_desktop,
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "restricted_token": self.restricted_token.to_dict(),
            "job_object": self.job_object.to_dict(),
            "low_integrity": self.low_integrity.to_dict(),
            "acl": self.acl.to_dict(),
            "firewall": self.firewall.to_dict(),
            "private_desktop": self.private_desktop.to_dict(),
        }


@dataclass(frozen=True)
class WindowsSandboxSetup:
    sandbox_accounts: WindowsCapabilityState
    login_ui_visibility: WindowsCapabilityState
    logon_rights: WindowsCapabilityState
    group_membership: WindowsCapabilityState
    state_dir_acl: WindowsCapabilityState
    acl_boundary: WindowsCapabilityState
    offline_network_filter: WindowsCapabilityState
    private_desktop: WindowsCapabilityState
    execution_backends: WindowsCapabilityState
    legacy_assets: WindowsCapabilityState

    def values(self) -> tuple[WindowsCapabilityState, ...]:
        return (
            self.sandbox_accounts,
            self.login_ui_visibility,
            self.logon_rights,
            self.group_membership,
            self.state_dir_acl,
            self.acl_boundary,
            self.offline_network_filter,
            self.private_desktop,
            self.execution_backends,
            self.legacy_assets,
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "sandbox_accounts": self.sandbox_accounts.to_dict(),
            "login_ui_visibility": self.login_ui_visibility.to_dict(),
            "logon_rights": self.logon_rights.to_dict(),
            "group_membership": self.group_membership.to_dict(),
            "state_dir_acl": self.state_dir_acl.to_dict(),
            "acl_boundary": self.acl_boundary.to_dict(),
            "offline_network_filter": self.offline_network_filter.to_dict(),
            "private_desktop": self.private_desktop.to_dict(),
            "execution_backends": self.execution_backends.to_dict(),
            "legacy_assets": self.legacy_assets.to_dict(),
        }


@dataclass(frozen=True)
class WindowsSandboxExecution:
    account_sids: WindowsCapabilityState
    credentials: WindowsCapabilityState
    launchers: WindowsCapabilityState
    runner_smoke: WindowsCapabilityState
    network_probe: WindowsCapabilityState

    def values(self) -> tuple[WindowsCapabilityState, ...]:
        return (
            self.account_sids,
            self.credentials,
            self.launchers,
            self.runner_smoke,
            self.network_probe,
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "account_sids": self.account_sids.to_dict(),
            "credentials": self.credentials.to_dict(),
            "launchers": self.launchers.to_dict(),
            "runner_smoke": self.runner_smoke.to_dict(),
            "network_probe": self.network_probe.to_dict(),
        }


@dataclass(frozen=True)
class WindowsSandboxDoctorReport:
    implementation: str
    platform_supported: bool
    platform_status: str
    primitives: WindowsSandboxPrimitives
    setup: WindowsSandboxSetup
    execution: WindowsSandboxExecution
    available: bool
    enforcement_status: str
    blocking_requirements: tuple[str, ...]
    recommended_action: str
    diagnostics: tuple[dict[str, Any], ...] = ()

    @property
    def missing_requirements(self) -> tuple[str, ...]:
        return self.blocking_requirements

    @classmethod
    def ready_for_tests(cls) -> WindowsSandboxDoctorReport:
        ready = WindowsCapabilityState("available", True, "test verified", {"source": "test"})
        primitives = WindowsSandboxPrimitives(ready, ready, ready, ready, ready, ready)
        setup = WindowsSandboxSetup(
            ready, ready, ready, ready, ready, ready, ready, ready, ready, ready
        )
        execution = WindowsSandboxExecution(ready, ready, ready, ready, ready)
        return cls(
            implementation="elevated",
            platform_supported=True,
            platform_status="supported",
            primitives=primitives,
            setup=setup,
            execution=execution,
            available=True,
            enforcement_status="available",
            blocking_requirements=(),
            recommended_action="Windows sandbox is ready.",
            diagnostics=(),
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "schema_version": DOCTOR_SCHEMA_VERSION,
            "implementation": self.implementation,
            "platform_supported": self.platform_supported,
            "platform_status": self.platform_status,
            "primitives": self.primitives.to_dict(),
            "setup": self.setup.to_dict(),
            "execution": self.execution.to_dict(),
            "available": self.available,
            "enforcement_status": self.enforcement_status,
            "blocking_requirements": list(self.blocking_requirements),
            "missing_requirements": list(self.blocking_requirements),
            "recommended_action": self.recommended_action,
            "diagnostics": list(self.diagnostics),
        }

    @property
    def reason(self) -> str:
        if self.available:
            return "Windows sandbox backend is available."
        missing = ", ".join(self.blocking_requirements) or "unknown requirements"
        return f"backend_unavailable: Windows sandbox requirements are missing: {missing}."


@dataclass(frozen=True)
class WindowsSandboxSetupReport:
    status: str
    requested_operation: str
    requires_elevation: bool
    changed: bool
    completed_steps: tuple[str, ...]
    pending_steps: tuple[str, ...]
    failed_steps: tuple[dict[str, Any], ...]
    available_after_setup: bool
    message: str
    diagnostics: tuple[dict[str, Any], ...] = ()

    @classmethod
    def ready_for_tests(cls) -> WindowsSandboxSetupReport:
        return cls(
            status="ready",
            requested_operation="setup",
            requires_elevation=False,
            changed=False,
            completed_steps=SETUP_STEP_ORDER,
            pending_steps=(),
            failed_steps=(),
            available_after_setup=True,
            message="Windows sandbox setup is ready.",
            diagnostics=(),
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "schema_version": SETUP_SCHEMA_VERSION,
            "status": self.status,
            "requested_operation": self.requested_operation,
            "requires_elevation": self.requires_elevation,
            "changed": self.changed,
            "completed_steps": list(self.completed_steps),
            "pending_steps": list(self.pending_steps),
            "failed_steps": list(self.failed_steps),
            "available_after_setup": self.available_after_setup,
            "message": self.message,
            "diagnostics": list(self.diagnostics),
        }


@dataclass(frozen=True)
class WindowsSandboxCleanupReport:
    status: str
    requested_operation: str
    requires_elevation: bool
    changed: bool
    completed_steps: tuple[str, ...]
    failed_steps: tuple[dict[str, Any], ...]
    diagnostics: tuple[dict[str, Any], ...] = ()
    residual_audit: dict[str, int] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "schema_version": CLEANUP_SCHEMA_VERSION,
            "status": self.status,
            "requested_operation": self.requested_operation,
            "requires_elevation": self.requires_elevation,
            "changed": self.changed,
            "completed": self.status == "completed",
            "failed": bool(self.failed_steps),
            "completed_steps": list(self.completed_steps),
            "failed_steps": list(self.failed_steps),
            "diagnostics": list(self.diagnostics),
            "residual_audit": dict(self.residual_audit),
        }
