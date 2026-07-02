from __future__ import annotations

import ctypes
import importlib.util
import json
import os
import re
import secrets
import shutil
import socket
import subprocess
import sys
import sysconfig
import time
from collections.abc import Callable
from contextlib import suppress
from ctypes import wintypes
from dataclasses import dataclass, field
from datetime import UTC, datetime
from functools import lru_cache
from pathlib import Path
from types import SimpleNamespace
from typing import Any, ClassVar

from singularity.observability.redaction import TraceRedactor
from singularity.release.paths import resolve_user_data_paths
from singularity.sandbox.artifacts import SandboxArtifactCollector
from singularity.sandbox.environment import SandboxEnvironmentBuilder
from singularity.sandbox.exceptions import SandboxCapabilityError
from singularity.sandbox.filesystem import SandboxFilesystemManager, random_trace_id
from singularity.sandbox.models import (
    PreparedSandbox,
    SandboxCapabilities,
    SandboxEnvPolicy,
    SandboxFilesystemMode,
    SandboxFilesystemPolicy,
    SandboxNetworkMode,
    SandboxNetworkPolicy,
    SandboxProfile,
    SandboxProfileName,
    SandboxRequest,
    SandboxResourceLimits,
    SandboxResult,
    SandboxStatus,
    SandboxViolation,
)
from singularity.sandbox.windows_runner import (
    NETWORK_PROBE_ENDPOINTS,
    WindowsRunnerResult,
    WindowsRunnerSpec,
    WindowsSandboxRunner,
)

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
LOGIN_UI_USERLIST_KEY = (
    r"HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon\SpecialAccounts\UserList"
)
SECURITY_ATTESTATION_KEY = r"HKLM:\SOFTWARE\Singularity\WindowsSandbox"
SECURITY_ATTESTATION_VALUE = "SecurityAttestation"
SECURITY_ATTESTATION_SCHEMA_VERSION = "singularity.windows-sandbox.security/v1"
SECURITY_ATTESTATION_POLICY = "interactive-only-with-remote-network-service-batch-denied"
CRED_TYPE_GENERIC = 1
CRED_PERSIST_LOCAL_MACHINE = 2
NERR_SUCCESS = 0
NERR_USER_EXISTS = 2224
NERR_INVALID_COMPUTER = 2351
ERROR_INVALID_NAME = 123
NERR_USER_NOT_FOUND = 2221
NERR_GROUP_NOT_FOUND = 2220
NERR_INVALID_NAME = 2202
USER_PRIV_USER = 1
UF_SCRIPT = 0x0001
UF_DONT_EXPIRE_PASSWD = 0x10000
# LSA account-right management: CreateProcessWithLogonW requires the target
# account to hold SeInteractiveLogonRight ("Log On Locally"); SE_DENY rights
# override matching allow rights, so deny rights must also be removed.
POLICY_LOOKUP_NAMES = 0x00000800
POLICY_CREATE_ACCOUNT = 0x00000010
SE_INTERACTIVE_LOGON_NAME = "SeInteractiveLogonRight"
SE_BATCH_LOGON_NAME = "SeBatchLogonRight"
SE_NETWORK_LOGON_NAME = "SeNetworkLogonRight"
SE_REMOTE_INTERACTIVE_LOGON_NAME = "SeRemoteInteractiveLogonRight"
SE_SERVICE_LOGON_NAME = "SeServiceLogonRight"
SE_DENY_INTERACTIVE_LOGON_NAME = "SeDenyInteractiveLogonRight"
SE_DENY_BATCH_LOGON_NAME = "SeDenyBatchLogonRight"
SE_DENY_NETWORK_LOGON_NAME = "SeDenyNetworkLogonRight"
SE_DENY_REMOTE_INTERACTIVE_LOGON_NAME = "SeDenyRemoteInteractiveLogonRight"
SE_DENY_SERVICE_LOGON_NAME = "SeDenyServiceLogonRight"
SANDBOX_DENY_LOGON_RIGHTS = (
    SE_DENY_REMOTE_INTERACTIVE_LOGON_NAME,
    SE_DENY_NETWORK_LOGON_NAME,
    SE_DENY_SERVICE_LOGON_NAME,
    SE_DENY_BATCH_LOGON_NAME,
)
SANDBOX_UNNEEDED_ALLOW_LOGON_RIGHTS = (
    SE_REMOTE_INTERACTIVE_LOGON_NAME,
    SE_NETWORK_LOGON_NAME,
    SE_SERVICE_LOGON_NAME,
    SE_BATCH_LOGON_NAME,
)
NERR_MEMBER_IN_GROUP = 2118
ERROR_MEMBER_IN_ALIAS = 1378
ERROR_NOT_FOUND = 1168
STATUS_OBJECT_NAME_NOT_FOUND = 0xC0000034
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
        ready = _available("test verified", {"source": "test"})
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
            completed_steps=(
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
            ),
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


class WindowsSandboxBackend:
    def __init__(
        self,
        *,
        runner: Any | None = None,
        filesystem: SandboxFilesystemManager | None = None,
        artifact_collector: SandboxArtifactCollector | None = None,
        acl_applier: Callable[[Path, str], None] | None = None,
        doctor_provider: Callable[[], WindowsSandboxDoctorReport] | None = None,
        setup_provider: Callable[[], WindowsSandboxSetupReport] | None = None,
        cleanup_provider: Callable[[], WindowsSandboxCleanupReport] | None = None,
        run_root_provider: Callable[[SandboxRequest], Path] | None = None,
    ) -> None:
        self.runner = runner or WindowsSandboxRunner()
        self.filesystem = filesystem or SandboxFilesystemManager()
        self.artifact_collector = artifact_collector or SandboxArtifactCollector()
        self._acl_applier = acl_applier or self._apply_run_acl
        self._doctor_provider = doctor_provider or probe_windows_sandbox
        self._setup_provider = setup_provider or setup_windows_sandbox
        self._cleanup_provider = cleanup_provider or cleanup_windows_sandbox_assets
        self._run_root_provider = run_root_provider or (
            lambda request: _windows_state_dir_path() / "runs" / request.sandbox_id
        )

    def name(self) -> str:
        return "windows"

    def doctor(self) -> WindowsSandboxDoctorReport:
        return self._doctor_provider()

    def setup(self) -> WindowsSandboxSetupReport:
        return self._setup_provider()

    def cleanup_assets(self) -> WindowsSandboxCleanupReport:
        return self._cleanup_provider()

    def is_available(self) -> bool:
        return self.doctor().available

    def capabilities(self) -> SandboxCapabilities:
        if not self.is_available():
            return SandboxCapabilities(
                filesystem_isolation=False,
                copy_on_write=False,
                readonly_mount=False,
                network_isolation=False,
                env_isolation=False,
                process_tree_kill=False,
                timeout=False,
                output_limit=False,
                memory_limit=False,
                process_limit=False,
                artifact_capture=False,
                change_detection=False,
            )
        return SandboxCapabilities(
            filesystem_isolation=True,
            copy_on_write=True,
            readonly_mount=True,
            network_isolation=True,
            env_isolation=True,
            process_tree_kill=True,
            timeout=True,
            output_limit=True,
            memory_limit=False,
            process_limit=False,
            artifact_capture=True,
            change_detection=True,
        )

    def prepare(self, request: SandboxRequest) -> PreparedSandbox:
        timing: dict[str, float] = {}
        phase_started = time.perf_counter()
        report = self.doctor()
        timing["sandbox_doctor_readiness_time_seconds"] = time.perf_counter() - phase_started
        if not report.available:
            raise SandboxCapabilityError(report.reason)
        phase_started = time.perf_counter()
        identity = _sandbox_identity_for_mode(request.profile.network.mode)
        timing["sandbox_account_selection_time_seconds"] = time.perf_counter() - phase_started
        if _external_writable_paths(request):
            raise SandboxCapabilityError(
                "backend_unavailable: Windows sandbox additional writable directories "
                "outside the workspace require an ACL lease/projection that is not available."
            )
        if request.profile.filesystem.readonly_paths:
            raise SandboxCapabilityError(
                "backend_unavailable: Windows sandbox path-specific readonly leases "
                "are not available."
            )
        filesystem_policy = request.profile.filesystem
        original_sandbox_root = filesystem_policy.sandbox_root
        filesystem_policy.sandbox_root = self._run_root_provider(request)
        try:
            phase_started = time.perf_counter()
            fs = self.filesystem.prepare_filesystem(
                sandbox_id=request.sandbox_id,
                policy=filesystem_policy,
                cwd=request.cwd,
            )
            timing["workspace_materialization_time_seconds"] = time.perf_counter() - phase_started
        finally:
            filesystem_policy.sandbox_root = original_sandbox_root
        phase_started = time.perf_counter()
        self._acl_applier(fs.sandbox_root, identity.account_name)
        timing["acl_grant_time_seconds"] = time.perf_counter() - phase_started
        env = SandboxEnvironmentBuilder().build_env(request.profile.env, os.environ)
        env = self._runtime_env(env)
        trace_id = random_trace_id()
        baseline = self.filesystem.capture_baseline(fs.workspace_copy_root)
        spec_path = fs.sandbox_root / "runner-spec.json"
        result_path = fs.sandbox_root / "runner-result.json"
        spec = WindowsRunnerSpec(
            command=_resolve_command(request.command, env=env),
            cwd=str(fs.execution_cwd),
            env=env,
            timeout_seconds=request.profile.resources.timeout_seconds,
            max_output_chars=request.profile.resources.max_output_chars,
            network_mode=request.profile.network.mode.value,
            result_path=str(result_path),
        )
        spec_path.write_text(
            json.dumps(spec.to_dict(), ensure_ascii=False, indent=2),
            encoding="utf-8",
        )
        return PreparedSandbox(
            sandbox_id=request.sandbox_id,
            backend_name=self.name(),
            sandbox_root=fs.sandbox_root,
            workspace_copy_root=fs.workspace_copy_root,
            execution_cwd=fs.execution_cwd,
            env=env,
            request=request,
            created_at=_now(),
            trace_id=trace_id,
            baseline={
                "files": baseline,
                "runner_spec": str(spec_path),
                "runner_result": str(result_path),
                "sandbox_account": identity.account_name,
                "credential_target": identity.credential_target,
                "sandbox_role": identity.role,
                "timing": timing,
            },
        )

    def run(self, prepared: PreparedSandbox) -> SandboxResult:
        started = time.perf_counter()
        timing = dict(prepared.baseline.get("timing") or {})
        phase_started = time.perf_counter()
        enforcement = self._runtime_enforcement_report()
        timing["sandbox_doctor_readiness_time_seconds"] = (
            timing.get("sandbox_doctor_readiness_time_seconds", 0.0)
            + time.perf_counter()
            - phase_started
        )
        if not enforcement.available and not _can_ignore_unrelated_network_probe_blocker(
            prepared,
            enforcement,
        ):
            now = _now()
            return SandboxResult(
                sandbox_id=prepared.sandbox_id,
                backend_name=self.name(),
                status=SandboxStatus.BACKEND_UNAVAILABLE,
                exit_code=None,
                stdout="",
                stderr=enforcement.reason,
                started_at=now,
                ended_at=now,
                duration_ms=int((time.perf_counter() - started) * 1000),
                trace_id=prepared.trace_id,
                cleanup_status="not_started",
                metadata={
                    "error_code": "backend_unavailable",
                    "reason": enforcement.reason,
                    "enforcement_status": enforcement.enforcement_status,
                    "blocking_requirements": list(enforcement.blocking_requirements),
                },
            )
        phase_started = time.perf_counter()
        runner_result = self.runner.run(prepared)
        timing["command_runtime_time_seconds"] = time.perf_counter() - phase_started
        timing.update(dict(runner_result.metadata.get("timing") or {}))
        phase_started = time.perf_counter()
        stdout = TraceRedactor().redact_text(runner_result.stdout)
        stderr = TraceRedactor().redact_text(runner_result.stderr)
        stdout, stderr, backend_output_truncated = _limit_output(
            stdout,
            stderr,
            prepared.request.profile.resources.max_output_chars,
        )
        timing["output_collection_time_seconds"] = (
            timing.get("output_collection_time_seconds", 0.0)
            + time.perf_counter()
            - phase_started
        )
        runner_metadata = dict(runner_result.metadata)
        restricted_token = bool(runner_metadata.get("restricted_token"))
        low_integrity = bool(runner_metadata.get("low_integrity"))
        private_desktop = bool(runner_metadata.get("private_desktop"))
        process_tree_kill = bool(runner_metadata.get("job_object"))
        network_filter_verified = (
            prepared.request.profile.network.mode != SandboxNetworkMode.DENIED
            or enforcement.setup.offline_network_filter.ready
        )
        network_probe_verified = (
            prepared.request.profile.network.mode != SandboxNetworkMode.DENIED
            or _network_probe_state_for_role(enforcement.execution.network_probe, "offline").ready
        )
        network_denied_verified = (
            prepared.request.profile.network.mode != SandboxNetworkMode.DENIED
            or (
                bool(runner_result.network_denied_verified)
                and network_filter_verified
                and network_probe_verified
            )
        )
        process_enforcement_verified = (
            restricted_token and low_integrity and private_desktop and process_tree_kill
        )
        status = self._status_from_runner(
            prepared,
            runner_result,
            network_denied_verified=network_denied_verified,
            process_enforcement_verified=process_enforcement_verified,
        )
        violations = []
        metadata = {
            **runner_metadata,
            "error_code": None,
            "execution_backend": "account_restricted_token",
            "sandbox_role": str(prepared.baseline.get("sandbox_role") or "unknown"),
            "restricted_token": restricted_token,
            "low_integrity": low_integrity,
            "private_desktop": private_desktop,
            "process_tree_kill": process_tree_kill,
            "job_killed": runner_result.job_killed,
            "network_denied_verified": network_denied_verified,
            "network_filter_verified": network_filter_verified,
            "network_probe_verified": network_probe_verified,
            "output_truncated": runner_result.output_truncated or backend_output_truncated,
            "artifact_refs": [],
        }
        if status == SandboxStatus.VIOLATION and not process_enforcement_verified:
            metadata["error_code"] = "sandbox_enforcement_failed"
            violations.append(
                SandboxViolation(
                    violation_type="process_isolation",
                    message="Sandbox process isolation evidence was not verified.",
                    severity="error",
                    evidence={
                        "restricted_token": restricted_token,
                        "low_integrity": low_integrity,
                        "private_desktop": private_desktop,
                        "process_tree_kill": process_tree_kill,
                    },
                )
            )
        elif status == SandboxStatus.VIOLATION:
            metadata["error_code"] = "network_isolation_failed"
            violations.append(
                SandboxViolation(
                    violation_type="network",
                    message="Sandbox network denied self-test failed.",
                    severity="error",
                    evidence={
                        "network_denied_verified": network_denied_verified,
                        "network_filter_verified": network_filter_verified,
                        "network_probe_verified": network_probe_verified,
                    },
                )
            )
        phase_started = time.perf_counter()
        changes = self.filesystem.detect_changes(
            prepared.workspace_copy_root,
            dict(prepared.baseline.get("files") or {}),
        )
        timing["change_detection_time_seconds"] = time.perf_counter() - phase_started
        phase_started = time.perf_counter()
        artifacts = self.artifact_collector.collect(
            sandbox_id=prepared.sandbox_id,
            workspace_root=prepared.workspace_copy_root,
            artifact_root=prepared.sandbox_root / "artifacts",
            artifact_paths=prepared.request.profile.filesystem.artifact_paths,
            limits=prepared.request.profile.resources,
            stdout=stdout,
            stderr=stderr,
        )
        timing["artifact_collection_time_seconds"] = time.perf_counter() - phase_started
        metadata["timing"] = timing
        metadata["artifact_refs"] = [artifact.artifact_id for artifact in artifacts]
        return SandboxResult(
            sandbox_id=prepared.sandbox_id,
            backend_name=self.name(),
            status=status,
            exit_code=runner_result.exit_code if status != SandboxStatus.VIOLATION else None,
            stdout=stdout,
            stderr=stderr,
            started_at=runner_result.started_at or _now(),
            ended_at=runner_result.ended_at or _now(),
            duration_ms=runner_result.duration_ms or int((time.perf_counter() - started) * 1000),
            artifacts=artifacts,
            filesystem_changes=changes,
            violations=violations,
            trace_id=prepared.trace_id,
            cleanup_status="not_started",
            metadata=metadata,
        )

    def cleanup(self, prepared: PreparedSandbox) -> None:
        self._cleanup_workspace_as_sandbox_account(prepared)
        normalized = _normalize_run_root_for_cleanup(prepared.sandbox_root)
        if not normalized.ok:
            raise SandboxCapabilityError(
                "backend_unavailable: sandbox run root could not be normalized for cleanup."
            )
        self.filesystem.cleanup(prepared.sandbox_root)

    def _cleanup_workspace_as_sandbox_account(self, prepared: PreparedSandbox) -> None:
        if os.name != "nt" or not prepared.workspace_copy_root.exists():
            return
        command = _workspace_cleanup_command(prepared.workspace_copy_root)
        result_path = prepared.sandbox_root / "runner-cleanup-result.json"
        spec_path = prepared.sandbox_root / "runner-cleanup-spec.json"
        with suppress(OSError):
            result_path.unlink()
        spec = WindowsRunnerSpec(
            command=command,
            cwd=str(prepared.sandbox_root),
            env=prepared.env,
            timeout_seconds=30,
            max_output_chars=20000,
            network_mode=SandboxNetworkMode.ALLOWED.value,
            result_path=str(result_path),
            operation="workspace_cleanup",
        )
        spec_path.write_text(
            json.dumps(spec.to_dict(), ensure_ascii=False, indent=2),
            encoding="utf-8",
        )
        cleanup_request = SandboxRequest(
            sandbox_id=prepared.sandbox_id,
            session_id=prepared.request.session_id,
            task_id=prepared.request.task_id,
            action_id=f"{prepared.request.action_id}:cleanup",
            command=command,
            cwd=prepared.sandbox_root,
            workspace_root=prepared.sandbox_root,
            profile=SandboxProfile(
                name=SandboxProfileName.ISOLATED_VERIFICATION,
                filesystem=SandboxFilesystemPolicy(
                    mode=SandboxFilesystemMode.EMPTY_TEMP_WORKSPACE,
                    workspace_root=prepared.sandbox_root,
                    detect_changes=False,
                ),
                network=SandboxNetworkPolicy(mode=SandboxNetworkMode.ALLOWED),
                env=SandboxEnvPolicy(),
                resources=SandboxResourceLimits(timeout_seconds=30, max_output_chars=20000),
                description="Cleanup of the per-run Windows sandbox workspace.",
            ),
            policy_decision_id=prepared.request.policy_decision_id,
            policy_constraints=prepared.request.policy_constraints,
            reason="windows sandbox run-root cleanup",
            metadata={"purpose": "windows_run_root_cleanup"},
        )
        cleanup_prepared = PreparedSandbox(
            sandbox_id=prepared.sandbox_id,
            backend_name=prepared.backend_name,
            sandbox_root=prepared.sandbox_root,
            workspace_copy_root=prepared.workspace_copy_root,
            execution_cwd=prepared.sandbox_root,
            env=prepared.env,
            request=cleanup_request,
            created_at=_now(),
            trace_id=prepared.trace_id,
            baseline={
                "runner_spec": str(spec_path),
                "runner_result": str(result_path),
                "sandbox_account": prepared.baseline.get("sandbox_account"),
                "credential_target": prepared.baseline.get("credential_target"),
                "sandbox_role": prepared.baseline.get("sandbox_role"),
            },
        )
        cleanup_result = self.runner.run(cleanup_prepared)
        if cleanup_result.timed_out or cleanup_result.exit_code != 0:
            raise SandboxCapabilityError(
                "backend_unavailable: sandbox account workspace pre-cleanup failed."
            )

    def _runtime_enforcement_report(self) -> WindowsSandboxDoctorReport:
        if (
            type(self).doctor is WindowsSandboxBackend.doctor
            and self._doctor_provider is probe_windows_sandbox
        ):
            return _probe_windows_sandbox_uncached()
        return self.doctor()

    @staticmethod
    def _status_from_runner(
        prepared: PreparedSandbox,
        runner_result: WindowsRunnerResult,
        *,
        network_denied_verified: bool | None = None,
        process_enforcement_verified: bool = True,
    ) -> SandboxStatus:
        if runner_result.timed_out:
            return SandboxStatus.TIMEOUT
        if not process_enforcement_verified:
            return SandboxStatus.VIOLATION
        network_verified = (
            runner_result.network_denied_verified
            if network_denied_verified is None
            else network_denied_verified
        )
        if (
            prepared.request.profile.network.mode == SandboxNetworkMode.DENIED
            and not network_verified
        ):
            return SandboxStatus.VIOLATION
        if runner_result.exit_code is None:
            return SandboxStatus.FAILED
        return SandboxStatus.SUCCESS if runner_result.exit_code == 0 else SandboxStatus.FAILED

    def _apply_run_acl(self, sandbox_root: Path, account_name: str) -> None:
        if os.name != "nt":
            return
        run_acl = _apply_sandbox_control_dir_acl(
            sandbox_root,
            account_names=(account_name,),
            operation="run_root_acl",
        )
        if not run_acl.ok:
            raise SandboxCapabilityError(
                "backend_unavailable: sandbox ACL boundary could not be applied."
            )
        icacls = shutil.which("icacls")
        if icacls is None:
            raise SandboxCapabilityError(
                "backend_unavailable: sandbox ACL boundary could not be applied."
            )
        commands = (
            [
                icacls,
                str(sandbox_root / "workspace"),
                "/setintegritylevel",
                "(OI)(CI)L",
                "/T",
                "/C",
                "/Q",
            ],
        )
        for command in commands:
            result = _run_command(command)
            if result.returncode != 0:
                raise SandboxCapabilityError(
                    "backend_unavailable: sandbox ACL boundary could not be applied."
                )

    @staticmethod
    def _runtime_env(env: dict[str, str]) -> dict[str, str]:
        runtime = dict(env)
        for name in (
            "COMSPEC",
            "PATH",
            "PATHEXT",
            "SYSTEMDRIVE",
            "SYSTEMROOT",
            "TEMP",
            "TMP",
            "WINDIR",
        ):
            value = os.environ.get(name)
            if value is not None and name not in runtime:
                runtime[name] = value
        path_entries = [str(path) for path in _python_runtime_path_directories()]
        existing_path = runtime.get("PATH", "")
        for entry in existing_path.split(os.pathsep):
            if entry and entry not in path_entries:
                path_entries.append(entry)
        if path_entries:
            runtime["PATH"] = os.pathsep.join(path_entries)
        runtime.setdefault("PYTHONIOENCODING", "utf-8")
        return runtime


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


@lru_cache(maxsize=1)
def probe_windows_sandbox() -> WindowsSandboxDoctorReport:
    return _probe_windows_sandbox_uncached()


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


def setup_windows_sandbox() -> WindowsSandboxSetupReport:
    return _setup_windows_sandbox_v2()


def _setup_windows_sandbox_v2() -> WindowsSandboxSetupReport:
    if os.name != "nt":
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


def cleanup_windows_sandbox_assets() -> WindowsSandboxCleanupReport:
    if os.name != "nt":
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
    failed: list[dict[str, Any]] = []
    diagnostics: list[dict[str, Any]] = list(_legacy_artifact_diagnostics())

    asset_accounts = tuple(dict.fromkeys((*SANDBOX_ACCOUNTS, *LEGACY_SANDBOX_ACCOUNTS)))
    for target in asset_accounts:
        if not target:
            continue
        credential = _delete_credential(target)
        if credential.ok:
            changed = changed or bool(credential.details.get("changed"))
            completed.append(f"credential:{_hash_text(target)}")
        else:
            failed.append({"step": "credential", "reason": credential.reason, "details": credential.details})

    firewall = _delete_firewall_group()
    if firewall.ok:
        changed = changed or bool(firewall.details.get("changed"))
        completed.append("firewall_group")
    else:
        failed.append(
            {"step": "firewall_group", "reason": firewall.reason, "details": firewall.details}
        )

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
            failed.append({"step": "login_ui_visibility", "reason": visibility.reason, "details": visibility.details})

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
            failed.append({"step": "sandbox_account", "reason": account.reason, "details": account.details})

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


def _probe_windows_sandbox_uncached() -> WindowsSandboxDoctorReport:
    platform_supported = os.name == "nt"
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
    if os.name != "nt":
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
    if os.name != "nt":
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


def _acl_state(
    platform_supported: bool,
    identity: _WindowsSandboxIdentity,
) -> WindowsCapabilityState:
    if not platform_supported:
        return _missing("Windows ACL boundary requires Windows.", {"tool": "icacls"})
    state_dir = _windows_state_dir_path()
    root = state_dir / "acl-probe"
    sid = _account_sid(identity.account_name)
    if not sid or not _credential_state(identity).ready:
        return _missing(
            "ACL boundary probe requires sandbox account and credential.",
            {
                "probe": "acl_boundary",
                "state_dir_hash": _hash_path(state_dir),
                "probe_root_hash": _hash_path(root),
            },
        )
    try:
        state_dir = _windows_state_dir()
        root = state_dir / "acl-probe"
        root.mkdir(parents=True, exist_ok=True)
        allowed = root / "allowed"
        denied = root / "denied"
        allowed.mkdir(parents=True, exist_ok=True)
        denied.mkdir(parents=True, exist_ok=True)
        control = _apply_probe_root_acl(
            root,
            account_names=(identity.account_name,),
            operation="acl_probe_control_acl",
            low_integrity_root=allowed,
        )
        if not control.ok:
            return _missing(
                "ACL probe control directory setup failed.",
                {
                    **_probe_evidence("acl_probe_control_acl", state_dir=state_dir, probe_root=root, path=root),
                    "reason": control.reason,
                    "details": control.details,
                },
            )
        grant = _apply_probe_root_acl(
            allowed,
            account_names=(identity.account_name,),
            operation="acl_probe_allowed_acl",
        )
        icacls = shutil.which("icacls")
        if icacls is None:
            return _missing(
                "icacls is required for ACL probe.",
                _probe_evidence(
                    "acl_probe_icacls_missing",
                    state_dir=state_dir,
                    probe_root=root,
                    path=denied,
                    extra={"tool": "icacls"},
                ),
            )
        deny = _run_command(
            [
                icacls,
                str(denied),
                "/inheritance:r",
                "/remove:g",
                identity.account_name,
                "/T",
                "/C",
            ],
        )
        if not grant.ok or deny.returncode != 0:
            details = grant.details if not grant.ok else _completed_process_diagnostics(
                "acl_probe_deny_icacls",
                deny,
                state_dir=state_dir,
                probe_root=root,
                path=denied,
            )
            return _missing(
                "ACL probe setup failed.",
                {
                    **_probe_evidence("acl_probe_setup", state_dir=state_dir, probe_root=root),
                    "grant_ok": grant.ok,
                    "deny_exit": deny.returncode,
                    "details": details,
                },
            )
        allowed_result = _account_python_smoke(
            identity=identity,
            cwd=allowed,
            code="from pathlib import Path; Path('ok.txt').write_text('ok', encoding='utf-8')",
            timeout_seconds=5,
            operation_prefix="acl_allowed",
        )
        denied_result = _account_python_smoke(
            identity=identity,
            cwd=root,
            code=(
                "from pathlib import Path\n"
                f"target = Path({str(denied / 'blocked.txt')!r})\n"
                "try:\n"
                "    target.write_text('bad', encoding='utf-8')\n"
                "except OSError:\n"
                "    raise SystemExit(0)\n"
                "raise SystemExit(7)\n"
            ),
            timeout_seconds=5,
            operation_prefix="acl_denied",
        )
        ready = allowed_result.exit_code == 0 and denied_result.exit_code == 0
        evidence = _probe_evidence(
            "acl_boundary",
            state_dir=state_dir,
            probe_root=root,
            extra={
                "account_sid_hash": _hash_sid(sid),
                "allowed": _runner_result_summary(
                    "acl_allowed_write",
                    allowed_result,
                    state_dir=state_dir,
                    probe_root=root,
                    path=allowed,
                ),
                "denied": _runner_result_summary(
                    "acl_denied_write",
                    denied_result,
                    state_dir=state_dir,
                    probe_root=root,
                    path=denied,
                ),
            },
        )
        return _state_from_bool(
            ready,
            "ACL boundary self-test passed for sandbox account.",
            "ACL boundary self-test failed for sandbox account.",
            evidence,
        )
    except OSError as exc:
        return _missing(
            "ACL probe directory could not be created.",
            _exception_diagnostics(
                "acl_probe_root_mkdir",
                exc,
                state_dir=state_dir,
                probe_root=root,
                path=root,
            ),
        )
    finally:
        _cleanup_probe_root(root)


def _network_state(sid: str) -> WindowsCapabilityState:
    if os.name != "nt":
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
    if os.name != "nt":
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
    if os.name != "nt":
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
    if os.name != "nt":
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


def _runner_smoke_state(identity: _WindowsSandboxIdentity) -> WindowsCapabilityState:
    if os.name != "nt":
        return _missing("Windows runner smoke requires Windows.", {"runner": "windows_runner.py"})
    runner = _runner_state()
    if not runner.ready:
        return runner
    sid = _account_sid(identity.account_name)
    if not _credential_state(identity).ready or not sid:
        state_dir = _windows_state_dir_path()
        return _missing(
            "Windows runner smoke requires sandbox account and credential.",
            _probe_evidence(
                "runner_smoke_prerequisites",
                state_dir=state_dir,
                probe_root=state_dir / "runner-smoke",
                extra={"runner": "windows_runner.py"},
            ),
        )
    state_dir = _windows_state_dir_path()
    root = state_dir / "runner-smoke"
    try:
        state_dir = _windows_state_dir()
        root = state_dir / "runner-smoke"
        root.mkdir(parents=True, exist_ok=True)
        acl = _apply_probe_root_acl(
            root,
            account_names=(identity.account_name,),
            operation="runner_smoke_acl",
        )
        if not acl.ok:
            return _missing(
                "Windows runner smoke ACL setup failed.",
                {
                    **_probe_evidence("runner_smoke_acl", state_dir=state_dir, probe_root=root),
                    "runner": "windows_runner.py",
                    "reason": acl.reason,
                    "details": acl.details,
                },
            )
        spec_path = root / "runner-spec.json"
        result_path = root / "runner-result.json"
        spec = WindowsRunnerSpec(
            command=[sys.executable, "-c", "print('sandbox-smoke')"],
            cwd=str(root),
            env=WindowsSandboxBackend._runtime_env({}),
            timeout_seconds=5,
            max_output_chars=2000,
            network_mode="allowed",
            result_path=str(result_path),
        )
        try:
            spec_path.write_text(json.dumps(spec.to_dict(), ensure_ascii=False), encoding="utf-8")
        except OSError as exc:
            return _missing(
                "Windows runner smoke spec could not be written.",
                _exception_diagnostics(
                    "runner_smoke_spec_write",
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
                "Windows account-backed runner smoke failed.",
                _account_runner_launch_exception_diagnostics(
                    "runner_smoke",
                    exc,
                    state_dir=state_dir,
                    probe_root=root,
                    path=root,
                ),
            )
        account_sid_hash = result.metadata.get("account_sid_hash")
        account_identity_verified = bool(account_sid_hash) and account_sid_hash == _hash_sid(sid)
        ready = (
            result.exit_code == 0
            and "sandbox-smoke" in result.stdout
            and bool(result.metadata.get("restricted_token"))
            and bool(result.metadata.get("low_integrity"))
            and bool(result.metadata.get("private_desktop"))
            and bool(result.metadata.get("job_object"))
            and account_identity_verified
        )
        return _state_from_bool(
            ready,
            "Windows account-backed runner smoke passed.",
            "Windows account-backed runner smoke failed.",
            _runner_result_summary(
                _runner_result_operation("runner_smoke", result),
                result,
                state_dir=state_dir,
                probe_root=root,
                path=root,
                extra={"account_identity_verified": account_identity_verified},
            ),
        )
    except Exception as exc:
        return _missing(
            "Windows account-backed runner smoke failed.",
            _account_runner_launch_exception_diagnostics(
                "runner_smoke",
                exc,
                state_dir=state_dir,
                probe_root=root,
                path=root,
            ),
        )
    finally:
        _cleanup_probe_root(root)


def _account_python_smoke(
    *,
    identity: _WindowsSandboxIdentity,
    cwd: Path,
    code: str,
    timeout_seconds: int,
    operation_prefix: str = "account_python_smoke",
) -> WindowsRunnerResult:
    spec_path = cwd / "runner-spec.json"
    result_path = cwd / "runner-result.json"
    for path in (spec_path, result_path):
        with suppress(FileNotFoundError):
            path.unlink()
    spec = WindowsRunnerSpec(
        command=[sys.executable, "-c", code],
        cwd=str(cwd),
        env=WindowsSandboxBackend._runtime_env({}),
        timeout_seconds=timeout_seconds,
        max_output_chars=2000,
        network_mode="allowed",
        result_path=str(result_path),
    )
    state_dir = _windows_state_dir_path()
    try:
        spec_path.write_text(json.dumps(spec.to_dict(), ensure_ascii=False), encoding="utf-8")
    except OSError as exc:
        return _probe_failure_runner_result(
            _exception_diagnostics(
                f"{operation_prefix}_spec_write",
                exc,
                state_dir=state_dir,
                probe_root=cwd,
                path=spec_path,
            )
        )
    prepared = SimpleNamespace(
        sandbox_root=cwd,
        baseline={
            "runner_spec": str(spec_path),
            "runner_result": str(result_path),
            "sandbox_account": identity.account_name,
            "credential_target": identity.credential_target,
            "sandbox_role": identity.role,
        },
        request=SimpleNamespace(
            profile=SimpleNamespace(resources=SandboxResourceLimits(timeout_seconds=timeout_seconds))
        ),
    )
    try:
        return WindowsSandboxRunner(
            account_name=identity.account_name,
            credential_target=identity.credential_target,
        ).run(prepared)
    except Exception as exc:
        return _probe_failure_runner_result(
            _account_runner_launch_exception_diagnostics(
                operation_prefix,
                exc,
                state_dir=state_dir,
                probe_root=cwd,
                path=cwd,
            )
        )


def _python_runtime_smoke_diagnostics(
    identities: tuple[_WindowsSandboxIdentity, ...],
) -> tuple[dict[str, Any], ...]:
    if os.name != "nt":
        return ()
    state_dir = _windows_state_dir_path()
    if not state_dir.exists():
        return ()
    root = state_dir / "python-runtime-smoke"
    try:
        root = _windows_state_dir() / "python-runtime-smoke"
        root.mkdir(parents=True, exist_ok=True)
        acl = _apply_probe_root_acl(
            root,
            account_names=tuple(identity.account_name for identity in identities),
            operation="python_runtime_smoke_acl",
        )
        if not acl.ok:
            return (
                {
                    "kind": "python_runtime_environment_blocker",
                    "status": "blocked",
                    "reason": "Python runtime smoke ACL setup failed.",
                    "evidence": {
                        **_probe_evidence("python_runtime_smoke_acl", state_dir=state_dir, probe_root=root),
                        "details": acl.details,
                    },
                },
            )
        diagnostics: list[dict[str, Any]] = []
        for identity in identities:
            sid = _account_sid(identity.account_name)
            cwd = root / identity.role
            cwd.mkdir(parents=True, exist_ok=True)
            role_acl = _apply_probe_root_acl(
                cwd,
                account_names=(identity.account_name,),
                operation=f"python_runtime_smoke_{identity.role}_acl",
            )
            if not role_acl.ok:
                diagnostics.append(
                    {
                        "kind": "python_runtime_environment_blocker",
                        "status": "blocked",
                        "reason": "Python runtime smoke role directory ACL setup failed.",
                        "evidence": {
                            **_probe_evidence(
                                f"python_runtime_smoke_{identity.role}_acl",
                                state_dir=state_dir,
                                probe_root=root,
                                path=cwd,
                                extra={
                                    "sandbox_role": identity.role,
                                    "network_mode": identity.network_mode.value,
                                    "target": "probe_root_acl",
                                },
                            ),
                            "details": role_acl.details,
                        },
                    }
                )
                continue
            result = _account_python_smoke(
                identity=identity,
                cwd=cwd,
                code=_PYTHON_RUNTIME_SMOKE_CODE,
                timeout_seconds=5,
                operation_prefix="python_runtime_smoke",
            )
            if result.exit_code == 0:
                continue
            diagnostics.append(
                _python_runtime_smoke_diagnostic(
                    identity=identity,
                    sid=sid,
                    result=result,
                    state_dir=state_dir,
                    root=root,
                )
            )
        return tuple(diagnostics)
    except Exception as exc:
        return (
            {
                "kind": "python_runtime_environment_blocker",
                "status": "blocked",
                "reason": "Python runtime smoke failed before module import checks completed.",
                "evidence": _exception_diagnostics(
                    "python_runtime_smoke",
                    exc,
                    state_dir=state_dir,
                    probe_root=root,
                    path=root,
                ),
            },
        )
    finally:
        _cleanup_probe_root(root)


def _python_runtime_smoke_diagnostic(
    *,
    identity: _WindowsSandboxIdentity,
    sid: str,
    result: WindowsRunnerResult,
    state_dir: Path,
    root: Path,
) -> dict[str, Any]:
    payload = _python_runtime_smoke_payload(result.stdout)
    module_status = _python_runtime_module_status(payload)
    failure_type, module = _python_runtime_failure(payload, result)
    evidence = _runner_result_summary(
        _runner_result_operation("python_runtime_smoke", result),
        result,
        state_dir=state_dir,
        probe_root=root,
        path=root,
        extra={
            "role": identity.role,
            "network_mode": identity.network_mode.value,
            "account": _account_name_diagnostics(identity.account_name),
            "account_sid_hash": _hash_sid(sid),
            "module_status": module_status,
            "failure_type": failure_type,
            "module": module,
            "sandbox_role": identity.role,
            "restricted_token": result.metadata.get("restricted_token"),
            "low_integrity": result.metadata.get("low_integrity"),
            "private_desktop": result.metadata.get("private_desktop"),
            "job_object": result.metadata.get("job_object"),
            "runtime_target_hashes": _runtime_target_hashes(),
            "runtime_access": _diagnostic_payload(
                payload.get("runtime_access") if isinstance(payload.get("runtime_access"), dict) else {}
            ),
            "ssl": _diagnostic_payload(payload.get("ssl") if isinstance(payload.get("ssl"), dict) else {}),
        },
    )
    return {
        "kind": "python_runtime_environment_blocker",
        "status": "blocked",
        "failure_type": failure_type,
        "module": module,
        "sandbox_role": identity.role,
        "restricted_token": result.metadata.get("restricted_token"),
        "low_integrity": result.metadata.get("low_integrity"),
        "private_desktop": result.metadata.get("private_desktop"),
        "job_object": result.metadata.get("job_object"),
        "reason": "Sandbox account Python runtime smoke failed.",
        "evidence": evidence,
    }


def _python_runtime_smoke_payload(stdout: str) -> dict[str, Any]:
    try:
        payload = json.loads(stdout.strip().splitlines()[-1])
    except (IndexError, json.JSONDecodeError):
        return {}
    if not isinstance(payload, dict):
        return {}
    return payload


def _python_runtime_module_status(payload: dict[str, Any]) -> dict[str, str]:
    modules = payload.get("modules") if isinstance(payload.get("modules"), dict) else payload
    if not isinstance(modules, dict):
        return {}
    result: dict[str, str] = {}
    for name in ("_ssl", "ssl", "socket", "hashlib", "pathlib"):
        value = modules.get(name)
        if isinstance(value, dict):
            result[name] = str(value.get("status") or "unknown")
        elif isinstance(value, str):
            result[name] = value
    return result


def _python_runtime_failure(
    payload: dict[str, Any],
    result: WindowsRunnerResult,
) -> tuple[str, str]:
    modules_payload = payload.get("modules")
    modules: dict[str, Any] = modules_payload if isinstance(modules_payload, dict) else {}
    runtime_access_payload = payload.get("runtime_access")
    runtime_access: dict[str, Any] = runtime_access_payload if isinstance(runtime_access_payload, dict) else {}
    output = f"{result.stdout}\n{result.stderr}\n{_python_runtime_payload_text(payload)}".lower()
    if _module_failed(modules, "_ssl"):
        if "dll search path" in output:
            return "dll_search_path_failed", "_ssl"
        if "libssl" in output or "libcrypto" in output:
            return "openssl_dependency_dll_load_failed", "_ssl"
        if _looks_like_dll_initialization_failed(output):
            return "ssl_low_integrity_runtime_initialization_failed", "_ssl"
        return "_ssl.pyd_load_failed", "_ssl"
    if _module_failed(modules, "ssl"):
        if _access_failed(runtime_access, ("openssl_config", "openssl_providers")):
            return "openssl_provider_or_config_unreadable", "ssl"
        if _access_failed(runtime_access, ("certificate_paths",)):
            return "certificate_path_unreadable", "ssl"
        return "_ssl.pyd_load_failed", "ssl"
    if _access_failed(runtime_access, ("openssl_config", "openssl_providers")):
        return "openssl_provider_or_config_unreadable", "ssl"
    if _access_failed(runtime_access, ("certificate_paths",)):
        return "certificate_path_unreadable", "ssl"
    if _access_failed(runtime_access, ("temp", "tmp", "profile")):
        return "temp_or_profile_access_failed", "ssl"
    if "dll search path" in output:
        return "dll_search_path_failed", "_ssl"
    if "libssl" in output or "libcrypto" in output:
        return "openssl_dependency_dll_load_failed", "_ssl"
    if _looks_like_dll_initialization_failed(output):
        return "ssl_low_integrity_runtime_initialization_failed", "_ssl"
    if "_ssl" in output or "_ssl.pyd" in output:
        return "_ssl.pyd_load_failed", "_ssl"
    return "ssl_low_integrity_runtime_initialization_failed", "ssl"


def _module_failed(modules: dict[str, Any], name: str) -> bool:
    value = modules.get(name)
    if isinstance(value, dict):
        return str(value.get("status") or "").lower() == "failed"
    return str(value or "").lower() == "failed"


def _access_failed(runtime_access: dict[str, Any], names: tuple[str, ...]) -> bool:
    for name in names:
        value = runtime_access.get(name)
        if isinstance(value, dict):
            status = str(value.get("status") or "").lower()
            if status in {"failed", "missing"}:
                return True
    return False


def _looks_like_dll_initialization_failed(output: str) -> bool:
    return any(
        marker in output
        for marker in (
            "dll initialization",
            "initialization routine failed",
            "初始化例程失败",
            "出现了内部错误",
        )
    )


def _python_runtime_payload_text(value: Any) -> str:
    if isinstance(value, dict):
        return "\n".join(_python_runtime_payload_text(item) for item in value.values())
    if isinstance(value, list | tuple):
        return "\n".join(_python_runtime_payload_text(item) for item in value)
    if isinstance(value, str):
        return value
    return ""


def _runtime_target_hashes() -> list[str]:
    return [_hash_path(path) for path, _permission in _runner_runtime_acl_targets()]


def _diagnostic_payload(value: Any) -> Any:
    if isinstance(value, dict):
        return {str(key): _diagnostic_payload(item) for key, item in value.items()}
    if isinstance(value, list | tuple):
        return [_diagnostic_payload(item) for item in value]
    if isinstance(value, str):
        return _diagnostic_text(value)
    return value


_PYTHON_RUNTIME_SMOKE_CODE = r"""
import importlib
import json
import os
from pathlib import Path


def _hash_path(path):
    import hashlib

    return hashlib.sha256(str(Path(path).expanduser()).encode("utf-8")).hexdigest()[:16]


def _check_readable(path):
    if not path:
        return {"status": "missing"}
    try:
        target = Path(path)
        if not target.exists():
            return {"status": "missing", "path_hash": _hash_path(target)}
        if target.is_dir():
            next(iter(target.iterdir()), None)
        else:
            with target.open("rb") as handle:
                handle.read(1)
        return {"status": "passed", "path_hash": _hash_path(target)}
    except BaseException as exc:
        return {
            "status": "failed",
            "path_hash": _hash_path(path),
            "error_type": type(exc).__name__,
            "message": str(exc)[:200],
        }


def _runtime_roots():
    import sys

    roots = []
    for value in (sys.prefix, sys.base_prefix, sys.exec_prefix):
        if value:
            path = Path(value)
            if path.exists() and path not in roots:
                roots.append(path)
    return roots


def _check_many(paths):
    statuses = {}
    for path in paths:
        checked = _check_readable(path)
        statuses[checked.get("path_hash", _hash_path(path))] = checked["status"]
    return {"status": "failed" if "failed" in statuses.values() else "passed", "entries": statuses}


modules = {}
ok = True
for name in ("_ssl", "ssl", "socket", "hashlib", "pathlib"):
    try:
        module = importlib.import_module(name)
        state = {"status": "passed"}
        filename = getattr(module, "__file__", "")
        if filename:
            state["file_hash"] = _hash_path(filename)
        modules[name] = state
    except BaseException as exc:
        ok = False
        modules[name] = {
            "status": "failed",
            "error_type": type(exc).__name__,
            "message": str(exc)[:200],
        }

ssl_info = {}
runtime_access = {}
try:
    ssl = importlib.import_module("ssl")
    ssl_info["openssl_version"] = getattr(ssl, "OPENSSL_VERSION", "")
    paths = ssl.get_default_verify_paths()
    ssl_info["default_verify_paths"] = {
        name: _hash_path(value)
        for name, value in {
            "cafile": paths.cafile,
            "capath": paths.capath,
            "openssl_cafile": paths.openssl_cafile,
            "openssl_capath": paths.openssl_capath,
        }.items()
        if value
    }
    cert_status = {}
    for value in (paths.cafile, paths.capath, paths.openssl_cafile, paths.openssl_capath):
        if value:
            cert_status[str(_hash_path(value))] = _check_readable(value)["status"]
    runtime_access["certificate_paths"] = {
        "status": "failed" if "failed" in cert_status.values() else "passed",
        "entries": cert_status,
    }
except BaseException as exc:
    ssl_info["error_type"] = type(exc).__name__
    ssl_info["message"] = str(exc)[:200]

for env_name, key in (("OPENSSL_CONF", "openssl_config"), ("OPENSSL_MODULES", "openssl_providers")):
    runtime_access[key] = _check_readable(os.environ.get(env_name))
for env_name, key in (("TEMP", "temp"), ("TMP", "tmp"), ("USERPROFILE", "profile")):
    runtime_access[key] = _check_readable(os.environ.get(env_name))
openssl_dlls = []
openssl_configs = []
openssl_providers = []
for root in _runtime_roots():
    openssl_dlls.extend((root / "Library" / "bin").glob("libssl*.dll"))
    openssl_dlls.extend((root / "Library" / "bin").glob("libcrypto*.dll"))
    config = root / "Library" / "ssl" / "openssl.cnf"
    if config.exists():
        openssl_configs.append(config)
    openssl_providers.extend((root / "Library" / "lib" / "ossl-modules").glob("*.dll"))
runtime_access["openssl_dlls"] = _check_many(openssl_dlls)
if openssl_configs:
    runtime_access["openssl_config"] = _check_many(openssl_configs)
if openssl_providers:
    runtime_access["openssl_providers"] = _check_many(openssl_providers)

print(json.dumps({"modules": modules, "ssl": ssl_info, "runtime_access": runtime_access}, sort_keys=True))
raise SystemExit(0 if ok else 7)
""".strip()


def _network_probe_state(
    identity: _WindowsSandboxIdentity,
    sid: str,
) -> WindowsCapabilityState:
    if os.name != "nt":
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
            env=WindowsSandboxBackend._runtime_env({}),
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
    if os.name != "nt":
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
    if os.name != "nt":
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
    if os.name != "nt":
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
    if os.name != "nt":
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
    if os.name != "nt":
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
    if os.name != "nt":
        return False
    result = _run_powershell(
        f"if (Test-Path -LiteralPath {_ps_quote(SECURITY_ATTESTATION_KEY)}) "
        "{ exit 0 } else { exit 2 }"
    )
    return result.returncode == 0


def _delete_security_attestation() -> _OperationResult:
    if os.name != "nt":
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
    if os.name != "nt":
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


def _available(reason: str, evidence: dict[str, Any]) -> WindowsCapabilityState:
    return WindowsCapabilityState("available", True, reason, evidence)


def _missing(reason: str, evidence: dict[str, Any]) -> WindowsCapabilityState:
    return WindowsCapabilityState("missing", True, reason, evidence)


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
    if os.name != "nt":
        return False
    result = _run_powershell(
        "$key = "
        f"{_ps_quote(LOGIN_UI_USERLIST_KEY)}; "
        f"$name = {_ps_quote(account_name)}; "
        "$item = Get-ItemProperty -Path $key -Name $name -ErrorAction SilentlyContinue; "
        "if ($null -ne $item) { exit 0 }; exit 1"
    )
    return result.returncode == 0


def _has_windows_symbols(library: str, *symbols: str) -> bool:
    if os.name != "nt":
        return False
    try:
        dll = ctypes.WinDLL(library, use_last_error=True)
        return all(hasattr(dll, symbol) for symbol in symbols)
    except (AttributeError, OSError):
        return False


@dataclass(frozen=True)
class _OperationResult:
    ok: bool
    reason: str = ""
    details: dict[str, Any] = field(default_factory=dict)


class _USER_INFO_1(ctypes.Structure):
    _fields_: ClassVar[list[tuple[str, Any]]] = [
        ("usri1_name", wintypes.LPWSTR),
        ("usri1_password", wintypes.LPWSTR),
        ("usri1_password_age", wintypes.DWORD),
        ("usri1_priv", wintypes.DWORD),
        ("usri1_home_dir", wintypes.LPWSTR),
        ("usri1_comment", wintypes.LPWSTR),
        ("usri1_flags", wintypes.DWORD),
        ("usri1_script_path", wintypes.LPWSTR),
    ]


class _USER_INFO_1003(ctypes.Structure):
    _fields_: ClassVar[list[tuple[str, Any]]] = [("usri1003_password", wintypes.LPWSTR)]


class _CREDENTIALW(ctypes.Structure):
    _fields_: ClassVar[list[tuple[str, Any]]] = [
        ("Flags", wintypes.DWORD),
        ("Type", wintypes.DWORD),
        ("TargetName", wintypes.LPWSTR),
        ("Comment", wintypes.LPWSTR),
        ("LastWritten", wintypes.FILETIME),
        ("CredentialBlobSize", wintypes.DWORD),
        ("CredentialBlob", ctypes.POINTER(ctypes.c_ubyte)),
        ("Persist", wintypes.DWORD),
        ("AttributeCount", wintypes.DWORD),
        ("Attributes", wintypes.LPVOID),
        ("TargetAlias", wintypes.LPWSTR),
        ("UserName", wintypes.LPWSTR),
    ]


class _LSA_UNICODE_STRING(ctypes.Structure):
    _fields_: ClassVar[list[tuple[str, Any]]] = [
        ("Length", wintypes.USHORT),
        ("MaximumLength", wintypes.USHORT),
        ("Buffer", wintypes.LPWSTR),
    ]


class _LSA_OBJECT_ATTRIBUTES(ctypes.Structure):
    _fields_: ClassVar[list[tuple[str, Any]]] = [
        ("Length", wintypes.ULONG),
        ("RootDirectory", wintypes.HANDLE),
        ("ObjectName", ctypes.POINTER(_LSA_UNICODE_STRING)),
        ("Attributes", wintypes.ULONG),
        ("SecurityDescriptor", wintypes.LPVOID),
        ("SecurityQualityOfService", wintypes.LPVOID),
    ]


class _LOCALGROUP_MEMBERS_INFO_0(ctypes.Structure):
    _fields_: ClassVar[list[tuple[str, Any]]] = [("lgrmi0_sid", wintypes.LPVOID)]


def _netapi32():
    dll = ctypes.WinDLL("netapi32", use_last_error=True)
    dll.NetUserAdd.argtypes = [
        wintypes.LPCWSTR,
        wintypes.DWORD,
        wintypes.LPVOID,
        ctypes.POINTER(wintypes.DWORD),
    ]
    dll.NetUserAdd.restype = wintypes.DWORD
    dll.NetUserSetInfo.argtypes = [
        wintypes.LPCWSTR,
        wintypes.LPCWSTR,
        wintypes.DWORD,
        wintypes.LPVOID,
        ctypes.POINTER(wintypes.DWORD),
    ]
    dll.NetUserSetInfo.restype = wintypes.DWORD
    dll.NetUserDel.argtypes = [
        wintypes.LPCWSTR,
        wintypes.LPCWSTR,
    ]
    dll.NetUserDel.restype = wintypes.DWORD
    dll.NetLocalGroupAddMembers.argtypes = [
        wintypes.LPCWSTR,
        wintypes.LPCWSTR,
        wintypes.DWORD,
        wintypes.LPVOID,
        wintypes.DWORD,
    ]
    dll.NetLocalGroupAddMembers.restype = wintypes.DWORD
    return dll


def _advapi32():
    dll = ctypes.WinDLL("advapi32", use_last_error=True)
    dll.CredWriteW.argtypes = [ctypes.POINTER(_CREDENTIALW), wintypes.DWORD]
    dll.CredWriteW.restype = wintypes.BOOL
    dll.CredReadW.argtypes = [
        wintypes.LPCWSTR,
        wintypes.DWORD,
        wintypes.DWORD,
        ctypes.POINTER(ctypes.c_void_p),
    ]
    dll.CredReadW.restype = wintypes.BOOL
    dll.CredDeleteW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD]
    dll.CredDeleteW.restype = wintypes.BOOL
    dll.CredFree.argtypes = [ctypes.c_void_p]
    dll.CredFree.restype = None
    dll.LsaOpenPolicy.argtypes = [
        ctypes.POINTER(_LSA_UNICODE_STRING),
        ctypes.POINTER(_LSA_OBJECT_ATTRIBUTES),
        wintypes.DWORD,
        ctypes.POINTER(wintypes.HANDLE),
    ]
    dll.LsaOpenPolicy.restype = wintypes.ULONG
    dll.LsaClose.argtypes = [wintypes.HANDLE]
    dll.LsaClose.restype = wintypes.ULONG
    dll.LsaAddAccountRights.argtypes = [
        wintypes.HANDLE,
        wintypes.LPVOID,
        ctypes.POINTER(_LSA_UNICODE_STRING),
        wintypes.ULONG,
    ]
    dll.LsaAddAccountRights.restype = wintypes.ULONG
    dll.LsaEnumerateAccountRights.argtypes = [
        wintypes.HANDLE,
        wintypes.LPVOID,
        ctypes.POINTER(ctypes.POINTER(_LSA_UNICODE_STRING)),
        ctypes.POINTER(wintypes.ULONG),
    ]
    dll.LsaEnumerateAccountRights.restype = wintypes.ULONG
    dll.LsaRemoveAccountRights.argtypes = [
        wintypes.HANDLE,
        wintypes.LPVOID,
        wintypes.BOOL,
        ctypes.POINTER(_LSA_UNICODE_STRING),
        wintypes.ULONG,
    ]
    dll.LsaRemoveAccountRights.restype = wintypes.ULONG
    dll.LsaFreeMemory.argtypes = [wintypes.LPVOID]
    dll.LsaFreeMemory.restype = wintypes.ULONG
    dll.ConvertStringSidToSidW.argtypes = [
        wintypes.LPCWSTR,
        ctypes.POINTER(wintypes.LPVOID),
    ]
    dll.ConvertStringSidToSidW.restype = wintypes.BOOL
    return dll


def _create_sandbox_account(name: str, password: str) -> _OperationResult:
    if os.name != "nt":
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
    if os.name != "nt":
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
    if os.name != "nt":
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
    if os.name != "nt":
        return False
    credential_ptr = ctypes.c_void_p()
    if not _advapi32().CredReadW(target, CRED_TYPE_GENERIC, 0, ctypes.byref(credential_ptr)):
        return False
    _advapi32().CredFree(credential_ptr)
    return True


def _delete_credential(target: str) -> _OperationResult:
    if os.name != "nt":
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
    if os.name != "nt":
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


def _is_elevated() -> bool:
    if os.name != "nt":
        return False
    try:
        return bool(ctypes.windll.shell32.IsUserAnAdmin())
    except Exception:
        return False


def _account_exists(name: str) -> bool:
    return _run_net(["user", name]).returncode == 0


def _run_net(args: list[str]) -> subprocess.CompletedProcess[str]:
    executable = shutil.which("net")
    if executable is None:
        return subprocess.CompletedProcess(["net", *args], 1, "", "net command unavailable")
    return _run_command([executable, *args])


def _account_sid(name: str) -> str:
    if os.name != "nt":
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
    if os.name != "nt" or not sid_string:
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
    if os.name != "nt" or not sid_string:
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
    if os.name != "nt":
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
    if os.name != "nt":
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
    if os.name != "nt":
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
    if os.name != "nt":
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
    if os.name != "nt":
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


def _ensure_state_dir_acl() -> _OperationResult:
    if os.name != "nt":
        return _OperationResult(False, "State directory ACL hardening requires Windows.")
    try:
        state_dir = _windows_state_dir()
    except OSError as exc:
        return _OperationResult(
            False,
            "Windows sandbox state directory could not be created.",
            _exception_diagnostics("state_dir_acl_mkdir", exc, state_dir=_windows_state_dir_path()),
        )
    acl = _apply_sandbox_control_dir_acl(
        state_dir,
        operation="state_dir_acl",
    )
    if not acl.ok:
        return acl
    return _OperationResult(True, "state_dir_acl_applied", {"changed": True, "state_dir_hash": _hash_path(state_dir)})


def _python_runtime_roots() -> tuple[Path, ...]:
    candidates: list[Path] = []
    executable = Path(sys.executable).expanduser().resolve(strict=False)
    if executable:
        candidates.append(executable.parent)
    for value in (
        sys.prefix,
        sys.base_prefix,
        getattr(sys, "exec_prefix", ""),
        sysconfig.get_config_var("base"),
        sysconfig.get_config_var("installed_base"),
        sysconfig.get_config_var("prefix"),
        sysconfig.get_config_var("exec_prefix"),
    ):
        if value:
            candidates.append(Path(str(value)).expanduser().resolve(strict=False))
    return _unique_existing_paths(candidates)


def _unique_existing_paths(paths: list[Path] | tuple[Path, ...]) -> tuple[Path, ...]:
    unique: list[Path] = []
    for path in paths:
        resolved = path.expanduser().resolve(strict=False)
        if resolved.exists() and all(existing != resolved for existing in unique):
            unique.append(resolved)
    return tuple(unique)


def _python_runtime_path_directories() -> tuple[Path, ...]:
    directories: list[Path] = []
    executable = Path(sys.executable).expanduser().resolve(strict=False)
    if executable.parent.exists():
        directories.append(executable.parent)
    for path, _permission in _runner_runtime_acl_targets():
        candidate = path if path.is_dir() else path.parent
        if candidate.exists():
            directories.append(candidate)
    return _unique_existing_paths(directories)


def _runner_runtime_acl_targets() -> tuple[tuple[Path, str], ...]:
    targets: list[tuple[Path, str]] = []

    def add(path: Path, permission: str) -> None:
        resolved = path.expanduser().resolve(strict=False)
        if resolved.exists() and all(existing != resolved for existing, _permission in targets):
            targets.append((resolved, permission))

    executable = Path(sys.executable).expanduser().resolve(strict=False)
    if executable.parent.exists():
        add(executable.parent, "RX")

    roots = _python_runtime_roots()
    for root in roots:
        add(root / "DLLs", "(OI)(CI)RX")
        add(root / "Library" / "bin", "(OI)(CI)RX")
        add(root / "Library" / "ssl", "(OI)(CI)RX")
        add(root / "Library" / "lib" / "ossl-modules", "(OI)(CI)RX")
        for pattern in ("python*.dll",):
            for child in sorted(root.glob(pattern), key=lambda path: path.name.casefold()):
                if child.is_file():
                    add(child, "RX")
        for child in sorted((root / "DLLs").glob("*.pyd"), key=lambda path: path.name.casefold()):
            if child.name.casefold() in {"_ssl.pyd", "_hashlib.pyd", "_socket.pyd"}:
                add(child, "RX")
        for child in sorted((root / "Library" / "bin").glob("*.dll"), key=lambda path: path.name.casefold()):
            lowered = child.name.casefold()
            if lowered.startswith(("libssl", "libcrypto")):
                add(child, "RX")
        openssl_config = root / "Library" / "ssl" / "openssl.cnf"
        if openssl_config.exists():
            add(openssl_config, "RX")
        for provider in sorted(
            (root / "Library" / "lib" / "ossl-modules").glob("*.dll"),
            key=lambda path: path.name.casefold(),
        ):
            if provider.is_file():
                add(provider, "RX")

    for module_name in ("_ssl", "_hashlib", "_socket"):
        with suppress(Exception):
            spec = importlib.util.find_spec(module_name)
            origin = Path(str(spec.origin)).expanduser().resolve(strict=False) if spec and spec.origin else None
            if origin and origin.exists():
                add(origin.parent, "(OI)(CI)RX")
                add(origin, "RX")
    return tuple(targets)


def _runner_runtime_stale_acl_targets() -> tuple[Path, ...]:
    return _python_runtime_roots()


def _ensure_runner_runtime_access(
    account_names: tuple[str, ...] = SANDBOX_ACCOUNTS,
) -> _OperationResult:
    if os.name != "nt":
        return _OperationResult(False, "Runner runtime ACL setup requires Windows.")
    icacls = shutil.which("icacls")
    if icacls is None:
        return _OperationResult(False, "icacls is required for runner runtime ACL setup.")
    targets = _runner_runtime_acl_targets()
    details = {
        "runtime_target_hashes": [_hash_path(path) for path, _permission in targets],
        "account_name_hashes": [_hash_text(account) for account in account_names],
    }
    stale_cleanup = _remove_stale_runner_runtime_base_access(
        icacls,
        targets,
        account_names,
        details,
    )
    if stale_cleanup is not None:
        return stale_cleanup
    for path, permission in targets:
        command = [icacls, str(path), "/grant:r"]
        command.extend(f"{account}:{permission}" for account in account_names)
        command.extend(("/C", "/Q"))
        result = _run_command(command, timeout_seconds=120)
        if result.returncode != 0:
            return _OperationResult(
                False,
                "Failed to grant sandbox accounts read/execute access to the Python runtime.",
                _completed_process_diagnostics(
                    "runner_runtime_acl_grant",
                    result,
                    state_dir=_windows_state_dir_path(),
                    path=path,
                    extra=details,
                ),
            )
    return _OperationResult(
        True,
        "runner_runtime_access_ready",
        {"changed": bool(targets and account_names), **details},
    )


def _remove_stale_runner_runtime_base_access(
    icacls: str,
    targets: tuple[tuple[Path, str], ...],
    account_names: tuple[str, ...],
    details: dict[str, Any],
) -> _OperationResult | None:
    del targets
    for path in _runner_runtime_stale_acl_targets():
        command = [icacls, str(path), "/remove:g", *account_names, "/C", "/Q"]
        result = _run_command(command, timeout_seconds=120)
        if result.returncode != 0:
            return _OperationResult(
                False,
                "Failed to remove stale sandbox account access from the Python runtime root.",
                _completed_process_diagnostics(
                    "runner_runtime_acl_stale_cleanup",
                    result,
                    state_dir=_windows_state_dir_path(),
                    path=path,
                    extra=details,
                ),
            )
    return None


def _remove_runner_runtime_access(account_names: tuple[str, ...]) -> _OperationResult:
    if not account_names:
        return _OperationResult(True, "runner_runtime_access_not_present", {"changed": False})
    if os.name != "nt":
        return _OperationResult(False, "Runner runtime ACL cleanup requires Windows.")
    icacls = shutil.which("icacls")
    if icacls is None:
        return _OperationResult(False, "icacls is required for runner runtime ACL cleanup.")
    targets = _runner_runtime_acl_targets()
    details = {
        "runtime_target_hashes": [_hash_path(path) for path, _permission in targets],
        "account_name_hashes": [_hash_text(account) for account in account_names],
    }
    cleanup_targets = tuple(dict.fromkeys((*_runner_runtime_stale_acl_targets(), *(path for path, _permission in targets))))
    for path in cleanup_targets:
        command = [icacls, str(path), "/remove:g", *account_names, "/C", "/Q"]
        result = _run_command(command, timeout_seconds=120)
        if result.returncode != 0:
            return _OperationResult(
                False,
                "Failed to remove sandbox account access from the Python runtime.",
                _completed_process_diagnostics(
                    "runner_runtime_acl_cleanup",
                    result,
                    state_dir=_windows_state_dir_path(),
                    path=path,
                    extra=details,
                ),
            )
    return _OperationResult(
        True,
        "runner_runtime_access_removed",
        {"changed": bool(targets), **details},
    )


def _delete_firewall_rule(name: str) -> _OperationResult:
    if os.name != "nt":
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


def _delete_windows_state_dir() -> _OperationResult:
    path = _windows_state_dir_path()
    details = {"state_dir_hash": _hash_path(path)}
    if os.name != "nt":
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


def _delete_firewall_group() -> _OperationResult:
    if os.name != "nt":
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
    if os.name != "nt":
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
    if os.name != "nt":
        return False
    completed = _run_powershell(
        f"if (Get-NetFirewallRule -DisplayName {_ps_quote(name)} -ErrorAction SilentlyContinue) "
        "{ exit 0 }; exit 1"
    )
    return completed.returncode == 0


def _generate_account_password() -> str:
    return "Sg!" + secrets.token_urlsafe(32) + "9"


def _run_powershell(command: str) -> subprocess.CompletedProcess[str]:
    executable = shutil.which("powershell") or shutil.which("pwsh")
    if executable is None:
        return subprocess.CompletedProcess(["powershell"], 1, "", "PowerShell unavailable")
    return _run_command(
        [
            executable,
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            command,
        ]
    )


@lru_cache(maxsize=1)
def _current_process_sid() -> str:
    if os.name != "nt":
        return ""
    result = _run_powershell(
        "[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value"
    )
    value = (result.stdout or "").strip()
    if result.returncode != 0 or re.fullmatch(r"S-\d+(?:-\d+)+", value) is None:
        return ""
    return value


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


def _run_command(
    command: list[str],
    *,
    timeout_seconds: float = 20,
) -> subprocess.CompletedProcess[str]:
    try:
        return subprocess.run(
            command,
            text=True,
            capture_output=True,
            check=False,
            timeout=timeout_seconds,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        return subprocess.CompletedProcess(
            command,
            1,
            "",
            json.dumps(sandbox_exception_diagnostics("subprocess", exc), ensure_ascii=False),
        )


def _apply_account_acl(
    path: Path,
    *,
    account_names: tuple[str, ...] = SANDBOX_ACCOUNTS,
    low_integrity_root: Path | None = None,
) -> _OperationResult:
    if os.name != "nt":
        return _OperationResult(False, "Windows ACL setup requires Windows.")
    icacls = shutil.which("icacls")
    if icacls is None:
        return _OperationResult(False, "icacls is required for sandbox ACL setup.")
    low_integrity_target = low_integrity_root or path
    grant_args = [icacls, str(path), "/grant"]
    grant_args.extend(f"{account}:(OI)(CI)M" for account in account_names)
    grant_args.extend(("/T", "/C"))
    grant = _run_command(grant_args)
    if grant.returncode != 0:
        return _OperationResult(
            False,
            _safe_output(grant),
            _completed_process_diagnostics(
                "acl_grant",
                grant,
                state_dir=_windows_state_dir_path(),
                probe_root=path,
                path=path,
            ),
        )
    integrity = _run_command(
        [
            icacls,
            str(low_integrity_target),
            "/setintegritylevel",
            "(OI)(CI)L",
            "/T",
            "/C",
        ]
    )
    if integrity.returncode != 0:
        return _OperationResult(
            False,
            _safe_output(integrity),
            _completed_process_diagnostics(
                "acl_low_integrity",
                integrity,
                state_dir=_windows_state_dir_path(),
                probe_root=path,
                path=low_integrity_target,
            ),
        )
    return _OperationResult(True)


def _apply_sandbox_control_dir_acl(
    path: Path,
    *,
    account_names: tuple[str, ...] = SANDBOX_ACCOUNTS,
    operation: str = "sandbox_control_dir_acl",
    low_integrity_root: Path | None = None,
) -> _OperationResult:
    if os.name != "nt":
        return _OperationResult(False, "Windows sandbox control directory ACL setup requires Windows.")
    state_dir = _windows_state_dir_path().resolve(strict=False)
    target = path.resolve(strict=False)
    if not (_is_relative_to(target, state_dir) or target == state_dir):
        return _OperationResult(
            False,
            "Refusing to grant sandbox account access outside the Windows sandbox state directory.",
            _probe_evidence(
                f"{operation}_unsafe_target",
                state_dir=state_dir,
                probe_root=path,
                path=path,
                extra={"target": "sandbox_control_dir"},
            ),
        )
    host_sid = _current_process_sid()
    icacls = shutil.which("icacls")
    if not host_sid or icacls is None:
        return _OperationResult(
            False,
            "icacls and the current process SID are required for sandbox control directory ACL setup.",
            _probe_evidence(
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
    commands: list[tuple[str, list[str], Path]] = [
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
        "account_name_hashes": [_hash_text(account) for account in account_names],
    }
    for command_operation, command, command_path in commands:
        result = _run_command(command)
        if result.returncode != 0:
            return _OperationResult(
                False,
                _safe_output(result),
                _completed_process_diagnostics(
                    command_operation,
                    result,
                    state_dir=state_dir,
                    probe_root=path,
                    path=command_path,
                    extra=details,
                ),
            )
    return _OperationResult(
        True,
        f"{operation}_ready",
        {
            **_probe_evidence(
                operation,
                state_dir=state_dir,
                probe_root=path,
                path=path,
                extra=details,
            ),
            "changed": True,
        },
    )


def _apply_probe_root_acl(
    path: Path,
    *,
    account_names: tuple[str, ...] = SANDBOX_ACCOUNTS,
    operation: str = "probe_root_acl",
    low_integrity_root: Path | None = None,
) -> _OperationResult:
    return _apply_sandbox_control_dir_acl(
        path,
        account_names=account_names,
        operation=operation,
        low_integrity_root=low_integrity_root,
    )


def _safe_output(result: subprocess.CompletedProcess[str]) -> str:
    text = (result.stderr or result.stdout or "").strip()
    return TraceRedactor().redact_text(text)[:500] or f"exit {result.returncode}"


def sandbox_exception_diagnostics(operation: str, exc: BaseException) -> dict[str, Any]:
    return _exception_diagnostics(operation, exc, state_dir=_windows_state_dir_path())


def _state_dir_state() -> WindowsCapabilityState:
    path = _windows_state_dir_path()
    if not path.exists():
        return _missing(
            "Windows sandbox machine state directory is missing.",
            _probe_evidence("windows_state_dir_missing", state_dir=path, path=path),
        )
    return _available(
        "Windows sandbox machine state directory is available.",
        _probe_evidence("windows_state_dir", state_dir=path, path=path),
    )


def _windows_state_dir() -> Path:
    path = _windows_state_dir_path()
    path.mkdir(parents=True, exist_ok=True)
    return path


def _cleanup_probe_root(path: Path) -> None:
    state_dir = _windows_state_dir_path().resolve(strict=False)
    candidate = path.resolve(strict=False)
    if not _is_relative_to(candidate, state_dir) or candidate == state_dir:
        return
    if not candidate.exists():
        return
    icacls = shutil.which("icacls")
    attrib = shutil.which("attrib")
    if icacls:
        _run_command(
            [
                icacls,
                str(candidate),
                "/setintegritylevel",
                "(OI)(CI)M",
                "/T",
                "/C",
                "/Q",
            ]
        )
        _run_command([icacls, str(candidate), "/reset", "/T", "/C", "/Q"])
    if attrib:
        _run_command([attrib, "-R", "-S", "-H", str(candidate / "*"), "/S", "/D"])
    with suppress(OSError):
        shutil.rmtree(candidate)


def _windows_state_dir_path() -> Path:
    if os.name == "nt":
        program_data = os.environ.get("PROGRAMDATA")
        if program_data:
            return Path(program_data) / "Singularity" / "windows-sandbox"
        system_drive = os.environ.get("SYSTEMDRIVE") or "C:"
        return Path(system_drive + "\\ProgramData") / "Singularity" / "windows-sandbox"
    return resolve_user_data_paths().state_dir / "windows-sandbox"


def _probe_evidence(
    operation: str,
    *,
    state_dir: Path | None = None,
    probe_root: Path | None = None,
    path: Path | None = None,
    extra: dict[str, Any] | None = None,
) -> dict[str, Any]:
    evidence: dict[str, Any] = {
        "operation": operation,
        "elevated": _safe_is_elevated(),
    }
    if path is not None:
        evidence["path_hash"] = _hash_path(path)
    if state_dir is not None:
        evidence["state_dir_hash"] = _hash_path(state_dir)
    if probe_root is not None:
        evidence["probe_root_hash"] = _hash_path(probe_root)
    if extra:
        evidence.update(extra)
    return evidence


def _exception_diagnostics(
    operation: str,
    exc: BaseException,
    *,
    state_dir: Path | None = None,
    probe_root: Path | None = None,
    path: Path | None = None,
    extra: dict[str, Any] | None = None,
) -> dict[str, Any]:
    diagnostics = _probe_evidence(
        operation,
        state_dir=state_dir,
        probe_root=probe_root,
        path=path,
        extra=extra,
    )
    diagnostics["error_type"] = type(exc).__name__
    diagnostics["errno"] = getattr(exc, "errno", None)
    diagnostics["winerror"] = getattr(exc, "winerror", None)
    diagnostics["strerror"] = _diagnostic_text(str(getattr(exc, "strerror", "") or str(exc)), state_dir, probe_root, path)
    diagnostics["returncode"] = None
    diagnostics["stdout_summary"] = ""
    diagnostics["stderr_summary"] = diagnostics["strerror"]
    return diagnostics


def _completed_process_diagnostics(
    operation: str,
    result: subprocess.CompletedProcess[str],
    *,
    state_dir: Path | None = None,
    probe_root: Path | None = None,
    path: Path | None = None,
    extra: dict[str, Any] | None = None,
) -> dict[str, Any]:
    embedded = _embedded_subprocess_diagnostics(result.stderr)
    diagnostics = _probe_evidence(
        operation,
        state_dir=state_dir,
        probe_root=probe_root,
        path=path,
        extra=extra,
    )
    diagnostics["returncode"] = result.returncode
    diagnostics["stdout_summary"] = _diagnostic_text(
        str(embedded.get("stdout_summary") or result.stdout or "") if embedded else (result.stdout or ""),
        state_dir,
        probe_root,
        path,
    )
    diagnostics["stderr_summary"] = _diagnostic_text(
        str(embedded.get("stderr_summary") or embedded.get("strerror") or result.stderr or "")
        if embedded
        else (result.stderr or ""),
        state_dir,
        probe_root,
        path,
    )
    diagnostics["errno"] = embedded.get("errno") if embedded else None
    diagnostics["winerror"] = embedded.get("winerror") if embedded else None
    diagnostics["strerror"] = (
        _diagnostic_text(str(embedded.get("strerror") or ""), state_dir, probe_root, path) if embedded else ""
    )
    if embedded and embedded.get("operation"):
        diagnostics["subprocess_operation"] = embedded.get("operation")
    if embedded and embedded.get("error_type"):
        diagnostics["error_type"] = embedded.get("error_type")
    return diagnostics


def _runner_result_summary(
    operation: str,
    result: WindowsRunnerResult,
    *,
    state_dir: Path | None = None,
    probe_root: Path | None = None,
    path: Path | None = None,
    extra: dict[str, Any] | None = None,
) -> dict[str, Any]:
    summary = _probe_evidence(
        operation,
        state_dir=state_dir,
        probe_root=probe_root,
        path=path,
        extra=extra,
    )
    summary.update(
        {
            "returncode": result.exit_code,
            "stdout_summary": _diagnostic_text(result.stdout, state_dir, probe_root, path),
            "stderr_summary": _diagnostic_text(result.stderr, state_dir, probe_root, path),
            "restricted_process": result.metadata.get("restricted_token"),
            "low_integrity": result.metadata.get("low_integrity"),
            "private_desktop": result.metadata.get("private_desktop"),
            "job_object": result.metadata.get("job_object"),
            "job_killed": result.job_killed,
            "network_denied_verified": result.network_denied_verified,
            "runner_error_code": result.metadata.get("error_code"),
            "runner_error_type": result.metadata.get("error_type"),
            "account_sid_hash": result.metadata.get("account_sid_hash"),
        }
    )
    return summary


def _runner_result_operation(prefix: str, result: WindowsRunnerResult) -> str:
    error_code = str(result.metadata.get("error_code") or "")
    error_type = str(result.metadata.get("error_type") or "")
    stderr = result.stderr.lower()
    if error_code == "runner_result_missing":
        return f"{prefix}_result_write_missing"
    if "createprocesswithlogonw" in stderr:
        return f"{prefix}_create_process_with_logon"
    if "createprocessasuserw" in stderr:
        return f"{prefix}_create_process_as_user"
    if "createrestrictedtoken" in stderr:
        return f"{prefix}_restricted_token"
    if "settokeninformation" in stderr or "low integrity" in stderr:
        return f"{prefix}_low_integrity"
    if "createdesktopw" in stderr:
        return f"{prefix}_private_desktop"
    if "assignprocesstojobobject" in stderr:
        return f"{prefix}_job_object"
    if result.exit_code not in {0, None}:
        return f"{prefix}_child_exit_nonzero"
    if error_type:
        return f"{prefix}_runner_error"
    return prefix


def _is_create_process_with_logon_access_denied(exc: BaseException) -> bool:
    text = str(exc).lower()
    return "createprocesswithlogonw" in text and (
        getattr(exc, "winerror", None) == 5
        or getattr(exc, "errno", None) in {5, 13}
        or "access is denied" in text
        or "拒绝访问" in text
    )


def _account_runner_launch_exception_diagnostics(
    prefix: str,
    exc: BaseException,
    *,
    state_dir: Path,
    probe_root: Path,
    path: Path,
    extra: dict[str, Any] | None = None,
) -> dict[str, Any]:
    if _is_create_process_with_logon_access_denied(exc):
        evidence_extra = {"target": "working_directory"}
        if extra:
            evidence_extra.update(extra)
        return _exception_diagnostics(
            f"{prefix}_working_directory_access",
            exc,
            state_dir=state_dir,
            probe_root=probe_root,
            path=path,
            extra=evidence_extra,
        )
    return _exception_diagnostics(
        _runner_exception_operation(prefix, exc),
        exc,
        state_dir=state_dir,
        probe_root=probe_root,
        path=path,
        extra=extra,
    )


def _runner_exception_operation(prefix: str, exc: BaseException) -> str:
    text = str(exc).lower()
    if "createprocesswithlogonw" in text:
        return f"{prefix}_create_process_with_logon"
    if "createprocessasuserw" in text:
        return f"{prefix}_create_process_as_user"
    if "createrestrictedtoken" in text:
        return f"{prefix}_restricted_token"
    if "settokeninformation" in text or "low integrity" in text:
        return f"{prefix}_low_integrity"
    if "createdesktopw" in text:
        return f"{prefix}_private_desktop"
    if "assignprocesstojobobject" in text:
        return f"{prefix}_job_object"
    return f"{prefix}_launch"


def _probe_failure_runner_result(diagnostics: dict[str, Any]) -> WindowsRunnerResult:
    return WindowsRunnerResult(
        exit_code=None,
        stdout="",
        stderr=str(diagnostics.get("strerror") or diagnostics.get("error_type") or "probe failed"),
        timed_out=False,
        started_at=_now(),
        ended_at=_now(),
        duration_ms=0,
        output_truncated=False,
        job_killed=False,
        network_denied_verified=False,
        metadata={
            "error_code": diagnostics.get("operation"),
            "error_type": diagnostics.get("error_type"),
            "diagnostics": diagnostics,
        },
    )


def _safe_is_elevated() -> bool:
    try:
        return bool(_is_elevated())
    except Exception:
        return False


def _embedded_subprocess_diagnostics(text: str | None) -> dict[str, Any] | None:
    if not text:
        return None
    try:
        payload = json.loads(text)
    except json.JSONDecodeError:
        return None
    if not isinstance(payload, dict) or "operation" not in payload:
        return None
    return payload


def _diagnostic_text(
    text: str,
    state_dir: Path | None = None,
    probe_root: Path | None = None,
    path: Path | None = None,
) -> str:
    sanitized = TraceRedactor().redact_text(str(text).strip())
    for item in (state_dir, probe_root, path):
        if item is None:
            continue
        path_text = str(item)
        resolved_text = str(item.expanduser().resolve(strict=False))
        replacement = f"<path:{_hash_path(item)}>"
        candidates = {path_text, resolved_text, path_text.replace("\\", "/"), resolved_text.replace("\\", "/")}
        candidates.update({candidate.replace("\\", "\\\\") for candidate in list(candidates)})
        for candidate in candidates:
            if candidate:
                sanitized = sanitized.replace(candidate, replacement)
    sanitized = re.sub(
        r"(?i)\b[A-Z]:[\\/][^\s\"'<>|]+",
        lambda match: f"<path:{_hash_text(match.group(0))}>",
        sanitized,
    )
    for account in (*SANDBOX_ACCOUNTS, *LEGACY_SANDBOX_ACCOUNTS):
        sanitized = sanitized.replace(account, f"<account:{_hash_text(account)}>")
    sanitized = re.sub(r"\bS-\d(?:-\d+){2,}\b", lambda match: f"<sid:{_hash_text(match.group(0))}>", sanitized)
    return sanitized[:500]


def _hash_text(value: str) -> str:
    import hashlib

    return hashlib.sha256(value.encode("utf-8")).hexdigest()[:16]


def _hash_path(value: Path) -> str:
    return _hash_text(str(value.expanduser().resolve(strict=False)))


def _hash_sid(value: str) -> str:
    return _hash_text(value) if value else ""


def _firewall_local_user_sddl(sid: str) -> str:
    return f"D:(A;;CC;;;{sid})"


def _ps_quote(value: str) -> str:
    return "'" + value.replace("'", "''") + "'"


def _resolve_command(command: list[str] | str, *, env: dict[str, str]) -> list[str] | str:
    if isinstance(command, str):
        return command
    if not command:
        return command
    resolved = [str(part) for part in command]
    executable = resolved[0]
    if Path(executable).is_absolute():
        return resolved
    candidate = _resolve_executable(executable, env)
    if candidate is not None:
        resolved[0] = str(candidate)
    return resolved


def _resolve_executable(name: str, env: dict[str, str]) -> Path | None:
    if os.name != "nt":
        found = shutil.which(name, path=env.get("PATH") or os.environ.get("PATH"))
        return Path(found) if found else None
    search_path = env.get("PATH") or os.environ.get("PATH") or ""
    found = shutil.which(name, path=search_path)
    if found:
        return Path(found)
    suffixes = env.get("PATHEXT") or os.environ.get("PATHEXT") or ".COM;.EXE;.BAT;.CMD"
    if Path(name).suffix:
        return None
    for suffix in suffixes.split(";"):
        found = shutil.which(f"{name}{suffix}", path=search_path)
        if found:
            return Path(found)
    return None


def _external_writable_paths(request: SandboxRequest) -> list[Path]:
    workspace = Path(request.profile.filesystem.workspace_root).expanduser().resolve(strict=False)
    external: list[Path] = []
    for value in request.profile.filesystem.writable_paths:
        raw = Path(value).expanduser()
        candidate = raw if raw.is_absolute() else workspace / raw
        resolved = candidate.resolve(strict=False)
        if not _is_relative_to(resolved, workspace):
            external.append(resolved)
    return external


def _is_relative_to(child: Path, parent: Path) -> bool:
    try:
        child_key = os.path.normcase(os.path.normpath(str(child)))
        parent_key = os.path.normcase(os.path.normpath(str(parent)))
        return os.path.commonpath([child_key, parent_key]) == parent_key
    except ValueError:
        return False


def _limit_output(stdout: str, stderr: str, max_chars: int | None) -> tuple[str, str, bool]:
    if max_chars is None or len(stdout) + len(stderr) <= max_chars:
        return stdout, stderr, False
    stdout_budget = min(len(stdout), max_chars)
    stderr_budget = max(0, max_chars - stdout_budget)
    return stdout[:stdout_budget], stderr[:stderr_budget], True


def _now() -> str:
    return datetime.now(UTC).isoformat()
