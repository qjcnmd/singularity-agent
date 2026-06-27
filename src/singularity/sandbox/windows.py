from __future__ import annotations

import ctypes
import json
import os
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
SANDBOX_ACCOUNT = "SingularitySandboxRunner"
FIREWALL_RULE_GROUP = "Singularity Sandbox"
FIREWALL_RULE_NAME = "Singularity Sandbox Runner Outbound Block"
CRED_TYPE_GENERIC = 1
CRED_PERSIST_LOCAL_MACHINE = 2
NERR_SUCCESS = 0
NERR_USER_EXISTS = 2224
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
    failed_steps: tuple[dict[str, str], ...]
    available_after_setup: bool
    message: str

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
        )
    changed = False
    completed: list[str] = []
    pending: list[str] = []
    failed: list[dict[str, str]] = []
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
        )
    password = ""
    if not _account_exists(SANDBOX_ACCOUNT):
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
            failed.append({"step": "sandbox_account", "reason": result.reason})
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
                failed.append({"step": "credential", "reason": reset.reason})
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
        failed.append({"step": "acl_boundary", "reason": acl_state.reason})
    if _has_windows_symbols("user32", "CreateDesktopW", "CloseDesktop"):
        completed.append("private_desktop")
    else:
        failed.append({"step": "private_desktop", "reason": "CreateDesktopW is unavailable"})
    execution_state = _runner_smoke_state()
    if execution_state.ready:
        completed.append("execution_backend")
    else:
        failed.append({"step": "execution_backend", "reason": execution_state.reason})
    probe_windows_sandbox.cache_clear()
    doctor = _probe_windows_sandbox_uncached()
    if doctor.execution.network_probe.ready:
        completed.append("network_probe")
    else:
        failed.append({"step": "network_probe", "reason": doctor.execution.network_probe.reason})
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
        message="Windows sandbox setup completed." if doctor.available else doctor.reason,
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
        recommended_action=(
            "Windows sandbox is ready."
            if available
            else "Run `singularity-agent sandbox setup --json` from an elevated shell and rerun doctor."
        ),
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
    root = _windows_state_dir() / "acl-probe"
    sid = _account_sid(SANDBOX_ACCOUNT)
    if not sid or not _credential_state().ready:
        return _missing(
            "ACL boundary probe requires sandbox account and credential.",
            {"probe": "acl_boundary"},
        )
    try:
        root.mkdir(parents=True, exist_ok=True)
        allowed = root / "allowed"
        denied = root / "denied"
        allowed.mkdir(parents=True, exist_ok=True)
        denied.mkdir(parents=True, exist_ok=True)
        grant = _apply_account_acl(allowed)
        icacls = shutil.which("icacls")
        if icacls is None:
            return _missing("icacls is required for ACL probe.", {"tool": "icacls"})
        deny = subprocess.run(
            [icacls, str(denied), "/inheritance:r", "/remove:g", SANDBOX_ACCOUNT, "/T", "/C"],
            text=True,
            capture_output=True,
            check=False,
            timeout=20,
        )
        if not grant.ok or deny.returncode != 0:
            return _missing(
                "ACL probe setup failed.",
                {"grant_ok": grant.ok, "deny_exit": deny.returncode},
            )
        allowed_result = _account_python_smoke(
            cwd=allowed,
            code="from pathlib import Path; Path('ok.txt').write_text('ok', encoding='utf-8')",
            timeout_seconds=5,
        )
        denied_result = _account_python_smoke(
            cwd=denied,
            code=(
                "from pathlib import Path\n"
                "try:\n"
                "    Path('blocked.txt').write_text('bad', encoding='utf-8')\n"
                "except OSError:\n"
                "    raise SystemExit(0)\n"
                "raise SystemExit(7)\n"
            ),
            timeout_seconds=5,
        )
        ready = allowed_result.exit_code == 0 and denied_result.exit_code == 0
        return _state_from_bool(
            ready,
            "ACL boundary self-test passed for sandbox account.",
            "ACL boundary self-test failed for sandbox account.",
            {
                "probe": "acl_boundary",
                "account_sid_hash": _hash_sid(sid),
                "allowed_exit": allowed_result.exit_code,
                "denied_exit": denied_result.exit_code,
            },
        )
    except OSError as exc:
        return _missing("ACL probe directory could not be created.", {"error_type": type(exc).__name__})


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
        return _missing(
            "Windows runner smoke requires sandbox account and credential.",
            {"runner": "windows_runner.py"},
        )
    root = _windows_state_dir() / "runner-smoke"
    try:
        root.mkdir(parents=True, exist_ok=True)
        acl = _apply_account_acl(root)
        if not acl.ok:
            return _missing(
                "Windows runner smoke ACL setup failed.",
                {"runner": "windows_runner.py", "reason": acl.reason},
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
        spec_path.write_text(json.dumps(spec.to_dict(), ensure_ascii=False), encoding="utf-8")
        prepared = SimpleNamespace(
            sandbox_root=root,
            baseline={"runner_spec": str(spec_path), "runner_result": str(result_path)},
            request=SimpleNamespace(profile=SimpleNamespace(resources=SandboxResourceLimits(timeout_seconds=5))),
        )
        result = WindowsSandboxRunner().run(prepared)
        return _state_from_bool(
            result.exit_code == 0
            and "sandbox-smoke" in result.stdout
            and bool(result.metadata.get("restricted_token"))
            and bool(result.metadata.get("low_integrity"))
            and bool(result.metadata.get("private_desktop"))
            and bool(result.metadata.get("job_object")),
            "Windows account-backed runner smoke passed.",
            "Windows account-backed runner smoke failed.",
            {
                "exit_code": result.exit_code,
                "restricted_token": result.metadata.get("restricted_token"),
                "low_integrity": result.metadata.get("low_integrity"),
                "private_desktop": result.metadata.get("private_desktop"),
                "job_object": result.metadata.get("job_object"),
            },
        )
    except Exception as exc:
        return _missing(
            "Windows account-backed runner smoke failed.",
            {"error_type": type(exc).__name__},
        )


def _account_python_smoke(
    *,
    cwd: Path,
    code: str,
    timeout_seconds: int,
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
    spec_path.write_text(json.dumps(spec.to_dict(), ensure_ascii=False), encoding="utf-8")
    prepared = SimpleNamespace(
        sandbox_root=cwd,
        baseline={"runner_spec": str(spec_path), "runner_result": str(result_path)},
        request=SimpleNamespace(
            profile=SimpleNamespace(resources=SandboxResourceLimits(timeout_seconds=timeout_seconds))
        ),
    )
    return WindowsSandboxRunner().run(prepared)


def _network_probe_state(sid: str) -> WindowsCapabilityState:
    if os.name != "nt":
        return _missing("Network probe requires Windows.", {"probe": "socket connect"})
    if not sid or not _network_state(sid).ready:
        return _missing("Network probe requires configured firewall rule.", {"probe": "socket connect"})
    host_baseline = _host_network_baseline_state()
    if not host_baseline.ready:
        return host_baseline
    try:
        root = _windows_state_dir() / "network-smoke"
        root.mkdir(parents=True, exist_ok=True)
        acl = _apply_account_acl(root)
        if not acl.ok:
            return _missing(
                "Network denied smoke ACL setup failed for sandbox account.",
                {"probe": "runtime", "reason": acl.reason},
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
        spec_path.write_text(json.dumps(spec.to_dict(), ensure_ascii=False), encoding="utf-8")
        prepared = SimpleNamespace(
            sandbox_root=root,
            baseline={"runner_spec": str(spec_path), "runner_result": str(result_path)},
            request=SimpleNamespace(profile=SimpleNamespace(resources=SandboxResourceLimits(timeout_seconds=5))),
        )
        result = WindowsSandboxRunner().run(prepared)
        return _state_from_bool(
            result.exit_code == 0 and result.network_denied_verified,
            "Network denied smoke passed for sandbox account.",
            "Network denied smoke failed for sandbox account.",
            {"probe": "runtime", "exit_code": result.exit_code},
        )
    except Exception as exc:
        return _missing(
            "Network denied smoke failed for sandbox account.",
            {"probe": "runtime", "error_type": type(exc).__name__},
        )


def _host_network_baseline_state() -> WindowsCapabilityState:
    if os.name != "nt":
        return _missing("Host outbound connectivity baseline requires Windows.", {"probe": "host_network"})
    for host, port in NETWORK_PROBE_ENDPOINTS:
        try:
            with socket.create_connection((host, int(port)), timeout=2):
                return _state_from_bool(
                    True,
                    "Host outbound connectivity baseline passed.",
                    "Host outbound connectivity baseline failed.",
                    {"probe": "host_network", "endpoint_hash": _hash_text(f"{host}:{port}")},
                )
        except OSError:
            continue
    return _missing(
        "Host outbound connectivity baseline failed; cannot prove sandbox firewall denial.",
        {"probe": "host_network"},
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
    info = _USER_INFO_1()
    info.usri1_name = name
    info.usri1_password = password
    info.usri1_priv = USER_PRIV_USER
    info.usri1_flags = UF_SCRIPT | UF_DONT_EXPIRE_PASSWD
    param_error = wintypes.DWORD()
    code = _netapi32().NetUserAdd(None, 1, ctypes.byref(info), ctypes.byref(param_error))
    if code in {NERR_SUCCESS, NERR_USER_EXISTS}:
        return _OperationResult(True)
    return _OperationResult(False, f"NetUserAdd failed: code {code}, param {param_error.value}")


def _set_account_password(name: str, password: str) -> _OperationResult:
    if os.name != "nt":
        return _OperationResult(False, "Windows account password update requires Windows.")
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
    return _OperationResult(False, f"NetUserSetInfo failed: code {code}, param {param_error.value}")


def _credential_exists(target: str) -> bool:
    if os.name != "nt":
        return False
    credential_ptr = ctypes.c_void_p()
    if not _advapi32().CredReadW(target, CRED_TYPE_GENERIC, 0, ctypes.byref(credential_ptr)):
        return False
    _advapi32().CredFree(credential_ptr)
    return True


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


def _generate_account_password() -> str:
    return "Sg!" + secrets.token_urlsafe(32) + "9"


def _credential_target() -> str:
    return "SingularitySandboxRunner"


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
        return subprocess.CompletedProcess(command, 1, "", str(exc))


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
        return _OperationResult(False, _safe_output(grant))
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
        return _OperationResult(False, _safe_output(integrity))
    return _OperationResult(True)


def _safe_output(result: subprocess.CompletedProcess[str]) -> str:
    text = (result.stderr or result.stdout or "").strip()
    return TraceRedactor().redact_text(text)[:500] or f"exit {result.returncode}"


def _windows_state_dir() -> Path:
    path = resolve_user_data_paths().state_dir / "windows-sandbox"
    path.mkdir(parents=True, exist_ok=True)
    return path


def _hash_text(value: str) -> str:
    import hashlib

    return hashlib.sha256(value.encode("utf-8")).hexdigest()[:16]


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
