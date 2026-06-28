from __future__ import annotations

import ctypes
import json
import os
import re
import secrets
import shutil
import socket
import subprocess
import sys
import time
from ctypes import wintypes
from dataclasses import dataclass, field
from datetime import UTC, datetime
from functools import lru_cache
from pathlib import Path
from types import SimpleNamespace
from typing import Any, Callable

from singularity.observability.redaction import TraceRedactor
from singularity.release.paths import resolve_user_data_paths
from singularity.sandbox.artifacts import SandboxArtifactCollector
from singularity.sandbox.environment import SandboxEnvironmentBuilder
from singularity.sandbox.exceptions import SandboxCapabilityError
from singularity.sandbox.filesystem import SandboxFilesystemManager, random_trace_id
from singularity.sandbox.models import (
    PreparedSandbox,
    SandboxCapabilities,
    SandboxNetworkMode,
    SandboxRequest,
    SandboxResult,
    SandboxResourceLimits,
    SandboxStatus,
    SandboxViolation,
)
from singularity.sandbox.windows_runner import (
    NETWORK_PROBE_ENDPOINTS,
    WindowsRunnerResult,
    WindowsRunnerSpec,
    WindowsSandboxRunner,
)


DOCTOR_SCHEMA_VERSION = "sandbox.windows.doctor/v1"
SETUP_SCHEMA_VERSION = "sandbox.windows.setup/v1"
WINDOWS_LOCAL_ACCOUNT_NAME_LIMIT = 20
SANDBOX_ACCOUNT = "SingularitySandbox"
LEGACY_SANDBOX_ACCOUNT = "SingularitySandboxRunner"
FIREWALL_RULE_GROUP = "Singularity Sandbox"
FIREWALL_RULE_NAME = "Singularity Sandbox Outbound Block"
LEGACY_FIREWALL_RULE_NAME = "Singularity Sandbox Runner Outbound Block"
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
    sandbox_account: WindowsCapabilityState
    acl_boundary: WindowsCapabilityState
    network_filter: WindowsCapabilityState
    private_desktop: WindowsCapabilityState
    execution_backend: WindowsCapabilityState

    def values(self) -> tuple[WindowsCapabilityState, ...]:
        return (
            self.sandbox_account,
            self.acl_boundary,
            self.network_filter,
            self.private_desktop,
            self.execution_backend,
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "sandbox_account": self.sandbox_account.to_dict(),
            "acl_boundary": self.acl_boundary.to_dict(),
            "network_filter": self.network_filter.to_dict(),
            "private_desktop": self.private_desktop.to_dict(),
            "execution_backend": self.execution_backend.to_dict(),
        }


@dataclass(frozen=True)
class WindowsSandboxExecution:
    account_sid: WindowsCapabilityState
    credential: WindowsCapabilityState
    launcher: WindowsCapabilityState
    runner_smoke: WindowsCapabilityState
    network_probe: WindowsCapabilityState

    def values(self) -> tuple[WindowsCapabilityState, ...]:
        return (
            self.account_sid,
            self.credential,
            self.launcher,
            self.runner_smoke,
            self.network_probe,
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "account_sid": self.account_sid.to_dict(),
            "credential": self.credential.to_dict(),
            "launcher": self.launcher.to_dict(),
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
    def ready_for_tests(cls) -> "WindowsSandboxDoctorReport":
        ready = _available("test verified", {"source": "test"})
        primitives = WindowsSandboxPrimitives(ready, ready, ready, ready, ready, ready)
        setup = WindowsSandboxSetup(ready, ready, ready, ready, ready)
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
    def ready_for_tests(cls) -> "WindowsSandboxSetupReport":
        return cls(
            status="ready",
            requested_operation="setup",
            requires_elevation=False,
            changed=False,
            completed_steps=(
                "sandbox_account",
                "credential",
                "acl_boundary",
                "network_filter",
                "private_desktop",
                "execution_backend",
                "network_probe",
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


class WindowsSandboxBackend:
    def __init__(
        self,
        *,
        runner: Any | None = None,
        filesystem: SandboxFilesystemManager | None = None,
        artifact_collector: SandboxArtifactCollector | None = None,
        acl_applier: Callable[[Path], None] | None = None,
        doctor_provider: Callable[[], WindowsSandboxDoctorReport] | None = None,
        setup_provider: Callable[[], WindowsSandboxSetupReport] | None = None,
    ) -> None:
        self.runner = runner or WindowsSandboxRunner()
        self.filesystem = filesystem or SandboxFilesystemManager()
        self.artifact_collector = artifact_collector or SandboxArtifactCollector()
        self._acl_applier = acl_applier or self._apply_run_acl
        self._doctor_provider = doctor_provider or probe_windows_sandbox
        self._setup_provider = setup_provider or setup_windows_sandbox

    def name(self) -> str:
        return "windows"

    def doctor(self) -> WindowsSandboxDoctorReport:
        return self._doctor_provider()

    def setup(self) -> WindowsSandboxSetupReport:
        return self._setup_provider()

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
        report = self.doctor()
        if not report.available:
            raise SandboxCapabilityError(report.reason)
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
        fs = self.filesystem.prepare_filesystem(
            sandbox_id=request.sandbox_id,
            policy=request.profile.filesystem,
            cwd=request.cwd,
        )
        self._acl_applier(fs.sandbox_root)
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
                "account": SANDBOX_ACCOUNT,
            },
        )

    def run(self, prepared: PreparedSandbox) -> SandboxResult:
        started = time.perf_counter()
        enforcement = self._runtime_enforcement_report()
        if not enforcement.available:
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
        runner_result = self.runner.run(prepared)
        stdout = TraceRedactor().redact_text(runner_result.stdout)
        stderr = TraceRedactor().redact_text(runner_result.stderr)
        stdout, stderr, backend_output_truncated = _limit_output(
            stdout,
            stderr,
            prepared.request.profile.resources.max_output_chars,
        )
        runner_metadata = dict(runner_result.metadata)
        restricted_token = bool(runner_metadata.get("restricted_token"))
        low_integrity = bool(runner_metadata.get("low_integrity"))
        private_desktop = bool(runner_metadata.get("private_desktop"))
        process_tree_kill = bool(runner_metadata.get("job_object"))
        network_filter_verified = (
            prepared.request.profile.network.mode != SandboxNetworkMode.DENIED
            or enforcement.setup.network_filter.ready
        )
        network_probe_verified = (
            prepared.request.profile.network.mode != SandboxNetworkMode.DENIED
            or enforcement.execution.network_probe.ready
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
            "sandbox_account": SANDBOX_ACCOUNT,
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
        changes = self.filesystem.detect_changes(
            prepared.workspace_copy_root,
            dict(prepared.baseline.get("files") or {}),
        )
        artifacts = self.artifact_collector.collect(
            sandbox_id=prepared.sandbox_id,
            workspace_root=prepared.workspace_copy_root,
            artifact_root=prepared.sandbox_root / "artifacts",
            artifact_paths=prepared.request.profile.filesystem.artifact_paths,
            limits=prepared.request.profile.resources,
            stdout=stdout,
            stderr=stderr,
        )
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
        self.filesystem.cleanup(prepared.sandbox_root)

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

    def _apply_run_acl(self, sandbox_root: Path) -> None:
        if os.name != "nt":
            return
        result = _apply_account_acl(
            sandbox_root,
            low_integrity_root=sandbox_root / "workspace",
        )
        if not result.ok:
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
        runtime.setdefault("PYTHONIOENCODING", "utf-8")
        return runtime


@lru_cache(maxsize=1)
def probe_windows_sandbox() -> WindowsSandboxDoctorReport:
    return _probe_windows_sandbox_uncached()


def setup_windows_sandbox() -> WindowsSandboxSetupReport:
    if os.name != "nt":
        return WindowsSandboxSetupReport(
            status="not_supported",
            requested_operation="setup",
            requires_elevation=False,
            changed=False,
            completed_steps=(),
            pending_steps=(),
            failed_steps=({"step": "platform", "reason": "Windows sandbox setup requires Windows."},),
            available_after_setup=False,
            message="Windows sandbox setup is not supported on this platform.",
            diagnostics=(),
        )
    changed = False
    completed: list[str] = []
    pending: list[str] = []
    failed: list[dict[str, Any]] = []
    if not _is_elevated():
        return WindowsSandboxSetupReport(
            status="requires_elevation",
            requested_operation="setup",
            requires_elevation=True,
            changed=False,
            completed_steps=(),
            pending_steps=(
                "sandbox_account",
                "credential",
                "network_filter",
                "acl_boundary",
                "execution_backend",
                "network_probe",
            ),
            failed_steps=(),
            available_after_setup=False,
            message="Run sandbox setup from an elevated shell to create account and firewall assets.",
            diagnostics=(),
        )
    diagnostics = _legacy_artifact_diagnostics()
    password = ""
    account_name_error = _validate_sandbox_account_name(SANDBOX_ACCOUNT)
    if account_name_error is not None:
        failed.append(account_name_error)
        return _setup_report(
            status="failed",
            changed=changed,
            completed=completed,
            failed=failed,
            available_after_setup=False,
            message=account_name_error["reason"],
            diagnostics=diagnostics,
        )
    elif not _account_exists(SANDBOX_ACCOUNT):
        password = _generate_account_password()
        result = _create_sandbox_account(SANDBOX_ACCOUNT, password)
        if result.ok:
            changed = True
            completed.append("sandbox_account")
            credential = _store_credential(password)
            if credential.ok:
                completed.append("credential")
            else:
                failed.append({"step": "credential", "reason": credential.reason})
        else:
            failed.append(
                _operation_failure_step(
                    "sandbox_account",
                    result,
                    account_name=SANDBOX_ACCOUNT,
                )
            )
    else:
        completed.append("sandbox_account")
        credential_state = _credential_state()
        if credential_state.ready:
            completed.append("credential")
        else:
            password = _generate_account_password()
            reset = _set_account_password(SANDBOX_ACCOUNT, password)
            if reset.ok:
                credential = _store_credential(password)
                if credential.ok:
                    changed = True
                    completed.append("credential")
                else:
                    failed.append({"step": "credential", "reason": credential.reason})
            else:
                failed.append(
                    _operation_failure_step(
                        "credential",
                        reset,
                        account_name=SANDBOX_ACCOUNT,
                    )
                )
    password = ""
    probe_windows_sandbox.cache_clear()
    if not _firewall_rule_ready():
        sid = _account_sid(SANDBOX_ACCOUNT)
        if sid:
            _run_powershell(
                f"Remove-NetFirewallRule -DisplayName {_ps_quote(FIREWALL_RULE_NAME)} "
                "-ErrorAction SilentlyContinue"
            )
            firewall_result = _run_powershell(
                "New-NetFirewallRule "
                f"-DisplayName '{FIREWALL_RULE_NAME}' "
                f"-Group '{FIREWALL_RULE_GROUP}' "
                "-Direction Outbound -Action Block -Enabled True "
                f"-LocalUser '{_firewall_local_user_sddl(sid)}' | Out-Null"
            )
            if firewall_result.returncode == 0:
                changed = True
                completed.append("network_filter")
            else:
                failed.append({"step": "network_filter", "reason": _safe_output(firewall_result)})
        else:
            failed.append({"step": "network_filter", "reason": "sandbox account SID unavailable"})
    else:
        completed.append("network_filter")
    acl_state = _acl_state(True)
    if acl_state.ready:
        completed.append("acl_boundary")
    else:
        failed.append({"step": "acl_boundary", "reason": acl_state.reason, "details": acl_state.evidence})
    if _has_windows_symbols("user32", "CreateDesktopW", "CloseDesktop"):
        completed.append("private_desktop")
    else:
        failed.append({"step": "private_desktop", "reason": "CreateDesktopW is unavailable"})
    execution_state = _runner_smoke_state()
    if execution_state.ready:
        completed.append("execution_backend")
    else:
        failed.append(
            {
                "step": "execution_backend",
                "reason": execution_state.reason,
                "details": execution_state.evidence,
            }
        )
    probe_windows_sandbox.cache_clear()
    doctor = _probe_windows_sandbox_uncached()
    if doctor.execution.network_probe.ready:
        completed.append("network_probe")
    else:
        failed.append(
            {
                "step": "network_probe",
                "reason": doctor.execution.network_probe.reason,
                "details": doctor.execution.network_probe.evidence,
            }
        )
    status = "ready" if doctor.available else "partial"
    if failed:
        status = "failed" if not completed else "partial"
    pending = [
        item
        for item in (
            "sandbox_account",
            "credential",
            "acl_boundary",
            "network_filter",
            "private_desktop",
            "execution_backend",
            "network_probe",
        )
        if item not in completed and not any(step.get("step") == item for step in failed)
    ]
    return WindowsSandboxSetupReport(
        status=status,
        requested_operation="setup",
        requires_elevation=False,
        changed=changed,
        completed_steps=tuple(dict.fromkeys(completed)),
        pending_steps=tuple(pending),
        failed_steps=tuple(failed),
        available_after_setup=doctor.available,
        message=_setup_message(doctor, diagnostics),
        diagnostics=diagnostics,
    )


def _setup_report(
    *,
    status: str,
    changed: bool,
    completed: list[str],
    failed: list[dict[str, Any]],
    available_after_setup: bool,
    message: str,
    diagnostics: tuple[dict[str, Any], ...],
) -> WindowsSandboxSetupReport:
    pending = [
        item
        for item in (
            "sandbox_account",
            "credential",
            "acl_boundary",
            "network_filter",
            "private_desktop",
            "execution_backend",
            "network_probe",
        )
        if item not in completed and not any(step.get("step") == item for step in failed)
    ]
    return WindowsSandboxSetupReport(
        status=status,
        requested_operation="setup",
        requires_elevation=False,
        changed=changed,
        completed_steps=tuple(dict.fromkeys(completed)),
        pending_steps=tuple(pending),
        failed_steps=tuple(failed),
        available_after_setup=available_after_setup,
        message=message,
        diagnostics=diagnostics,
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
    sid = _account_sid(SANDBOX_ACCOUNT) if platform_supported else ""
    diagnostics = _legacy_artifact_diagnostics() if platform_supported else ()
    state_dir = _state_dir_state() if platform_supported else None
    if state_dir is not None and not state_dir.ready:
        diagnostics = (*diagnostics, {"kind": "windows_sandbox_state_dir", **state_dir.to_dict()})
    setup = WindowsSandboxSetup(
        sandbox_account=_state_from_bool(
            bool(sid),
            "sandbox account exists",
            "sandbox account is missing",
            {"account": SANDBOX_ACCOUNT, "sid": _hash_sid(sid) if sid else None},
        ),
        acl_boundary=_acl_state(platform_supported),
        network_filter=_network_state(sid),
        private_desktop=_state_from_bool(
            primitives.private_desktop.ready,
            "private desktop primitive is available",
            "private desktop primitive is missing",
            {"api": "CreateDesktopW"},
        ),
        execution_backend=_execution_backend_state(primitives, sid),
    )
    execution = WindowsSandboxExecution(
        account_sid=_state_from_bool(
            bool(sid),
            "sandbox account SID resolved",
            "sandbox account SID unresolved",
            {"sid_hash": _hash_sid(sid) if sid else None},
        ),
        credential=_credential_state(),
        launcher=_state_from_bool(
            _has_windows_symbols("advapi32", "CreateProcessWithLogonW")
            and _has_windows_symbols("advapi32", "CreateProcessAsUserW"),
            "Windows account launcher primitive is available",
            "Windows account launcher primitive is missing",
            {"api": "CreateProcessWithLogonW/CreateProcessAsUserW"},
        ),
        runner_smoke=_runner_smoke_state(),
        network_probe=_network_probe_state(sid),
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


def _acl_state(platform_supported: bool) -> WindowsCapabilityState:
    if not platform_supported:
        return _missing("Windows ACL boundary requires Windows.", {"tool": "icacls"})
    state_dir = _windows_state_dir_path()
    root = state_dir / "acl-probe"
    sid = _account_sid(SANDBOX_ACCOUNT)
    if not sid or not _credential_state().ready:
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
        control = _apply_account_acl(root, low_integrity_root=allowed)
        if not control.ok:
            return _missing(
                "ACL probe control directory setup failed.",
                {
                    **_probe_evidence("acl_probe_control_acl", state_dir=state_dir, probe_root=root, path=root),
                    "reason": control.reason,
                    "details": control.details,
                },
            )
        grant = _apply_account_acl(allowed)
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
            [icacls, str(denied), "/inheritance:r", "/remove:g", SANDBOX_ACCOUNT, "/T", "/C"],
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
            cwd=allowed,
            code="from pathlib import Path; Path('ok.txt').write_text('ok', encoding='utf-8')",
            timeout_seconds=5,
            operation_prefix="acl_allowed",
        )
        denied_result = _account_python_smoke(
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
        {"rule": FIREWALL_RULE_NAME, "group": FIREWALL_RULE_GROUP, "local_user_sid_hash": _hash_sid(sid)},
    )


def _execution_backend_state(
    primitives: WindowsSandboxPrimitives,
    sid: str,
) -> WindowsCapabilityState:
    ready = (
        primitives.restricted_token.ready
        and primitives.job_object.ready
        and primitives.low_integrity.ready
        and primitives.private_desktop.ready
        and bool(sid)
        and _credential_state().ready
        and _runner_smoke_state().ready
    )
    return _state_from_bool(
        ready,
        "Windows account-backed execution smoke is available.",
        "Windows account-backed execution smoke is incomplete.",
        {"runner": "windows_runner.py", "account_sid_hash": _hash_sid(sid) if sid else None},
    )


def _credential_state() -> WindowsCapabilityState:
    # We intentionally do not read or print credential material. Presence is
    # tested through the Windows Credential Manager target only.
    if os.name != "nt":
        return _missing("Credential Manager probe requires Windows.", {"target": _credential_target()})
    ready = _credential_exists(_credential_target())
    return _state_from_bool(
        ready,
        "Sandbox credential target is present.",
        "Sandbox credential target is missing.",
        {"storage_scope": "windows_credential_manager", "target": _credential_target()},
    )


def _runner_state() -> WindowsCapabilityState:
    runner_path = Path(__file__).with_name("windows_runner.py")
    return _state_from_bool(
        runner_path.exists(),
        "Windows runner entrypoint exists.",
        "Windows runner entrypoint is missing.",
        {"runner_hash": _hash_text(str(runner_path))},
    )


def _runner_smoke_state() -> WindowsCapabilityState:
    if os.name != "nt":
        return _missing("Windows runner smoke requires Windows.", {"runner": "windows_runner.py"})
    runner = _runner_state()
    if not runner.ready:
        return runner
    if not _credential_state().ready or not _account_sid(SANDBOX_ACCOUNT):
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
        acl = _apply_account_acl(root)
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
            baseline={"runner_spec": str(spec_path), "runner_result": str(result_path)},
            request=SimpleNamespace(profile=SimpleNamespace(resources=SandboxResourceLimits(timeout_seconds=5))),
        )
        try:
            result = WindowsSandboxRunner().run(prepared)
        except Exception as exc:
            return _missing(
                "Windows account-backed runner smoke failed.",
                _exception_diagnostics(
                    _runner_exception_operation("runner_smoke", exc),
                    exc,
                    state_dir=state_dir,
                    probe_root=root,
                    path=root,
                ),
            )
        ready = (
            result.exit_code == 0
            and "sandbox-smoke" in result.stdout
            and bool(result.metadata.get("restricted_token"))
            and bool(result.metadata.get("low_integrity"))
            and bool(result.metadata.get("private_desktop"))
            and bool(result.metadata.get("job_object"))
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
            ),
        )
    except Exception as exc:
        return _missing(
            "Windows account-backed runner smoke failed.",
            _exception_diagnostics(
                _runner_exception_operation("runner_smoke", exc),
                exc,
                state_dir=state_dir,
                probe_root=root,
                path=root,
            ),
        )


def _account_python_smoke(
    *,
    cwd: Path,
    code: str,
    timeout_seconds: int,
    operation_prefix: str = "account_python_smoke",
) -> WindowsRunnerResult:
    spec_path = cwd / "runner-spec.json"
    result_path = cwd / "runner-result.json"
    for path in (spec_path, result_path):
        try:
            path.unlink()
        except FileNotFoundError:
            pass
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
        baseline={"runner_spec": str(spec_path), "runner_result": str(result_path)},
        request=SimpleNamespace(
            profile=SimpleNamespace(resources=SandboxResourceLimits(timeout_seconds=timeout_seconds))
        ),
    )
    try:
        return WindowsSandboxRunner().run(prepared)
    except Exception as exc:
        return _probe_failure_runner_result(
            _exception_diagnostics(
                _runner_exception_operation(operation_prefix, exc),
                exc,
                state_dir=state_dir,
                probe_root=cwd,
                path=cwd,
            )
        )


def _network_probe_state(sid: str) -> WindowsCapabilityState:
    if os.name != "nt":
        return _missing("Network probe requires Windows.", {"probe": "socket connect"})
    if not sid or not _network_state(sid).ready:
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
        acl = _apply_account_acl(root)
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
        spec = WindowsRunnerSpec(
            command=[sys.executable, "-c", "print('network-smoke')"],
            cwd=str(root),
            env=WindowsSandboxBackend._runtime_env({}),
            timeout_seconds=5,
            max_output_chars=2000,
            network_mode=SandboxNetworkMode.DENIED.value,
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
            baseline={"runner_spec": str(spec_path), "runner_result": str(result_path)},
            request=SimpleNamespace(profile=SimpleNamespace(resources=SandboxResourceLimits(timeout_seconds=5))),
        )
        try:
            result = WindowsSandboxRunner().run(prepared)
        except Exception as exc:
            return _missing(
                "Network denied smoke failed for sandbox account.",
                _exception_diagnostics(
                    "network_probe_runner_launch",
                    exc,
                    state_dir=state_dir,
                    probe_root=root,
                    path=root,
                ),
            )
        operation = (
            "network_probe"
            if result.exit_code == 0 and result.network_denied_verified
            else "network_probe_sandbox_network_not_blocked"
            if result.exit_code == 0
            else _runner_result_operation("network_probe", result)
        )
        return _state_from_bool(
            result.exit_code == 0 and result.network_denied_verified,
            "Network denied smoke passed for sandbox account.",
            "Network denied smoke failed for sandbox account.",
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
            _exception_diagnostics(
                "network_probe_runner_launch",
                exc,
                state_dir=state_dir,
                probe_root=root,
                path=root,
                extra={"probe": "runtime", "local_user_sid_hash": _hash_sid(sid)},
            ),
        )


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
        return f"{action} Legacy sandbox artifacts detected; review diagnostics before cleanup."
    return action


def _setup_message(
    doctor: WindowsSandboxDoctorReport,
    diagnostics: tuple[dict[str, Any], ...],
) -> str:
    message = "Windows sandbox setup completed." if doctor.available else doctor.reason
    if diagnostics:
        return f"{message} Legacy sandbox artifacts detected; review diagnostics before cleanup."
    return message


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


def _operation_failure_step(
    step: str,
    result: "_OperationResult",
    *,
    account_name: str,
) -> dict[str, Any]:
    details = dict(result.details)
    details.update(_account_name_diagnostics(account_name))
    details.setdefault("account_name_limit", WINDOWS_LOCAL_ACCOUNT_NAME_LIMIT)
    return {"step": step, "reason": result.reason, "details": details}


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
    if LEGACY_SANDBOX_ACCOUNT and LEGACY_SANDBOX_ACCOUNT != SANDBOX_ACCOUNT:
        if _account_exists(LEGACY_SANDBOX_ACCOUNT):
            diagnostics.append(
                {
                    "kind": "legacy_sandbox_account",
                    "status": "present",
                    **_account_name_diagnostics(LEGACY_SANDBOX_ACCOUNT),
                }
            )
        if _credential_exists(LEGACY_SANDBOX_ACCOUNT):
            diagnostics.append(
                {
                    "kind": "legacy_credential",
                    "status": "present",
                    "target_hash": _hash_text(LEGACY_SANDBOX_ACCOUNT),
                    "target_redacted": _redact_account_name(LEGACY_SANDBOX_ACCOUNT),
                }
            )
    if LEGACY_FIREWALL_RULE_NAME and LEGACY_FIREWALL_RULE_NAME != FIREWALL_RULE_NAME:
        if _firewall_rule_exists(LEGACY_FIREWALL_RULE_NAME):
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
    _fields_ = [
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
    _fields_ = [("usri1003_password", wintypes.LPWSTR)]


class _CREDENTIALW(ctypes.Structure):
    _fields_ = [
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
    dll.CredFree.argtypes = [ctypes.c_void_p]
    dll.CredFree.restype = None
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


def _credential_exists(target: str) -> bool:
    if os.name != "nt":
        return False
    credential_ptr = ctypes.c_void_p()
    if not _advapi32().CredReadW(target, CRED_TYPE_GENERIC, 0, ctypes.byref(credential_ptr)):
        return False
    _advapi32().CredFree(credential_ptr)
    return True


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


def _store_credential(password: str) -> _OperationResult:
    if os.name != "nt":
        return _OperationResult(False, "Windows Credential Manager requires Windows.")
    blob = password.encode("utf-16-le")
    blob_buffer = (ctypes.c_ubyte * len(blob)).from_buffer_copy(blob)
    credential = _CREDENTIALW()
    credential.Type = CRED_TYPE_GENERIC
    credential.TargetName = _credential_target()
    credential.UserName = SANDBOX_ACCOUNT
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


def _firewall_rule_ready() -> bool:
    sid = _account_sid(SANDBOX_ACCOUNT)
    return bool(sid and _network_state(sid).ready)


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


def _credential_target() -> str:
    return SANDBOX_ACCOUNT


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


def _run_command(command: list[str]) -> subprocess.CompletedProcess[str]:
    try:
        return subprocess.run(
            command,
            text=True,
            capture_output=True,
            check=False,
            timeout=20,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        return subprocess.CompletedProcess(
            command,
            1,
            "",
            json.dumps(sandbox_exception_diagnostics("subprocess", exc), ensure_ascii=False),
        )


def _apply_account_acl(path: Path, *, low_integrity_root: Path | None = None) -> _OperationResult:
    if os.name != "nt":
        return _OperationResult(False, "Windows ACL setup requires Windows.")
    icacls = shutil.which("icacls")
    if icacls is None:
        return _OperationResult(False, "icacls is required for sandbox ACL setup.")
    low_integrity_target = low_integrity_root or path
    grant = _run_command(
        [
            icacls,
            str(path),
            "/grant",
            f"{SANDBOX_ACCOUNT}:(OI)(CI)M",
            "/T",
            "/C",
        ]
    )
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


def _safe_output(result: subprocess.CompletedProcess[str]) -> str:
    text = (result.stderr or result.stdout or "").strip()
    return TraceRedactor().redact_text(text)[:500] or f"exit {result.returncode}"


def sandbox_exception_diagnostics(operation: str, exc: BaseException) -> dict[str, Any]:
    return _exception_diagnostics(operation, exc, state_dir=_windows_state_dir_path())


def _state_dir_state() -> WindowsCapabilityState:
    path = _windows_state_dir_path()
    try:
        _windows_state_dir()
    except OSError as exc:
        return _missing(
            "Windows sandbox machine state directory is unavailable.",
            _exception_diagnostics(
                "windows_state_dir_mkdir",
                exc,
                state_dir=path,
                path=path,
            ),
        )
    return _available(
        "Windows sandbox machine state directory is available.",
        _probe_evidence("windows_state_dir", state_dir=path, path=path),
    )


def _windows_state_dir() -> Path:
    path = _windows_state_dir_path()
    path.mkdir(parents=True, exist_ok=True)
    return path


def _windows_state_dir_path() -> Path:
    if os.name == "nt":
        program_data = os.environ.get("PROGRAMDATA")
        if program_data:
            return Path(program_data) / "Singularity" / "windows-sandbox"
        system_drive = os.environ.get("SystemDrive") or "C:"
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
