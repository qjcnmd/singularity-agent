from __future__ import annotations

import json
import os
import shutil
import time
from collections.abc import Callable
from contextlib import suppress
from pathlib import Path
from typing import Any

from singularity.observability.redaction import TraceRedactor
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
from singularity.sandbox.windows_acl import _apply_sandbox_control_dir_acl as _apply_run_root_acl
from singularity.sandbox.windows_cleanup import (
    _normalize_run_root_for_cleanup,
    _workspace_cleanup_command,
    cleanup_windows_sandbox_assets,
)
from singularity.sandbox.windows_common import (
    CLEANUP_SCHEMA_VERSION,
    DOCTOR_SCHEMA_VERSION,
    FIREWALL_RULE_GROUP,
    FIREWALL_RULE_NAME,
    LEGACY_FIREWALL_RULE_NAME,
    LEGACY_SANDBOX_ACCOUNT,
    LEGACY_SANDBOX_ACCOUNTS,
    LEGACY_SINGLE_SANDBOX_ACCOUNT,
    LOGIN_UI_USERLIST_KEY,
    OFFLINE_SANDBOX_ACCOUNT,
    ONLINE_SANDBOX_ACCOUNT,
    READINESS_SNAPSHOT_TTL_SECONDS,
    SANDBOX_ACCOUNTS,
    SECURITY_ATTESTATION_KEY,
    SECURITY_ATTESTATION_POLICY,
    SECURITY_ATTESTATION_SCHEMA_VERSION,
    SECURITY_ATTESTATION_VALUE,
    SETUP_SCHEMA_VERSION,
    SETUP_STEP_ORDER,
    WINDOWS_LOCAL_ACCOUNT_NAME_LIMIT,
    WindowsCapabilityState,
    WindowsSandboxCleanupReport,
    WindowsSandboxDoctorReport,
    WindowsSandboxExecution,
    WindowsSandboxPrimitives,
    WindowsSandboxSetup,
    WindowsSandboxSetupReport,
    _external_writable_paths,
    _limit_output,
    _now,
    _run_command,
    _sandbox_identity_for_mode,
    _windows_state_dir_path,
    sandbox_exception_diagnostics,
)
from singularity.sandbox.windows_doctor import (
    _can_ignore_unrelated_network_probe_blocker,
    _network_probe_state_for_role,
    _probe_windows_sandbox_uncached,
    probe_windows_sandbox,
    setup_windows_sandbox,
)
from singularity.sandbox.windows_platform import is_windows as _is_windows
from singularity.sandbox.windows_runner import (
    WindowsRunnerResult,
    WindowsRunnerSpec,
    WindowsSandboxRunner,
)
from singularity.sandbox.windows_runtime import _python_runtime_path_directories, _resolve_command

__all__ = [
    "CLEANUP_SCHEMA_VERSION",
    "DOCTOR_SCHEMA_VERSION",
    "FIREWALL_RULE_GROUP",
    "FIREWALL_RULE_NAME",
    "LEGACY_FIREWALL_RULE_NAME",
    "LEGACY_SANDBOX_ACCOUNT",
    "LEGACY_SANDBOX_ACCOUNTS",
    "LEGACY_SINGLE_SANDBOX_ACCOUNT",
    "LOGIN_UI_USERLIST_KEY",
    "OFFLINE_SANDBOX_ACCOUNT",
    "ONLINE_SANDBOX_ACCOUNT",
    "READINESS_SNAPSHOT_TTL_SECONDS",
    "SANDBOX_ACCOUNTS",
    "SECURITY_ATTESTATION_KEY",
    "SECURITY_ATTESTATION_POLICY",
    "SECURITY_ATTESTATION_SCHEMA_VERSION",
    "SECURITY_ATTESTATION_VALUE",
    "SETUP_SCHEMA_VERSION",
    "SETUP_STEP_ORDER",
    "WINDOWS_LOCAL_ACCOUNT_NAME_LIMIT",
    "WindowsCapabilityState",
    "WindowsSandboxBackend",
    "WindowsSandboxCleanupReport",
    "WindowsSandboxDoctorReport",
    "WindowsSandboxExecution",
    "WindowsSandboxPrimitives",
    "WindowsSandboxSetup",
    "WindowsSandboxSetupReport",
    "cleanup_windows_sandbox_assets",
    "probe_windows_sandbox",
    "sandbox_exception_diagnostics",
    "setup_windows_sandbox",
]


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
        self._readiness_snapshot: WindowsSandboxDoctorReport | None = None
        self._readiness_snapshot_at: float | None = None

    def name(self) -> str:
        return "windows"

    def doctor(self) -> WindowsSandboxDoctorReport:
        return self._doctor_provider()

    def setup(self) -> WindowsSandboxSetupReport:
        report = self._setup_provider()
        self._clear_readiness_snapshot()
        return report

    def cleanup_assets(self) -> WindowsSandboxCleanupReport:
        report = self._cleanup_provider()
        self._clear_readiness_snapshot()
        return report

    def is_available(self) -> bool:
        report, _elapsed = self._readiness_report()
        return report.available

    def capabilities(self) -> SandboxCapabilities:
        report, _elapsed = self._readiness_report()
        if not report.available:
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
        report, elapsed = self._readiness_report()
        timing["sandbox_doctor_readiness_time_seconds"] = elapsed
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
        sandbox_root = self._run_root_provider(request)
        filesystem_policy.sandbox_root = sandbox_root
        try:
            if sandbox_root.exists() and any(sandbox_root.iterdir()):
                self.filesystem.cleanup(sandbox_root)
            sandbox_root.mkdir(parents=True, exist_ok=True)
            phase_started = time.perf_counter()
            self._acl_applier(sandbox_root, identity.account_name)
            timing["acl_grant_time_seconds"] = time.perf_counter() - phase_started
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
        self._apply_workspace_low_integrity(fs.workspace_copy_root)
        timing["workspace_low_integrity_time_seconds"] = time.perf_counter() - phase_started
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
                "readiness": self._readiness_evidence(report),
                "timing": timing,
            },
        )

    def run(self, prepared: PreparedSandbox) -> SandboxResult:
        started = time.perf_counter()
        timing = dict(prepared.baseline.get("timing") or {})
        enforcement, elapsed = self._runtime_enforcement_report(prepared)
        timing["sandbox_doctor_readiness_time_seconds"] = (
            timing.get("sandbox_doctor_readiness_time_seconds", 0.0)
            + elapsed
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
        if not _is_windows() or not prepared.workspace_copy_root.exists():
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

    def _readiness_report(
        self,
        *,
        refresh: bool = False,
        force_uncached_default: bool = False,
    ) -> tuple[WindowsSandboxDoctorReport, float]:
        started = time.perf_counter()
        if not refresh and self._readiness_snapshot is not None and not self._readiness_snapshot_expired():
            return self._readiness_snapshot, time.perf_counter() - started
        if (
            force_uncached_default
            and type(self).doctor is WindowsSandboxBackend.doctor
            and self._doctor_provider is probe_windows_sandbox
        ):
            report = _probe_windows_sandbox_uncached()
        else:
            report = self.doctor()
        self._store_readiness_snapshot(report)
        return report, time.perf_counter() - started

    def _store_readiness_snapshot(self, report: WindowsSandboxDoctorReport) -> None:
        self._readiness_snapshot = report
        self._readiness_snapshot_at = time.perf_counter()

    def _clear_readiness_snapshot(self) -> None:
        self._readiness_snapshot = None
        self._readiness_snapshot_at = None

    def _readiness_snapshot_expired(self) -> bool:
        if self._readiness_snapshot_at is None:
            return True
        return (time.perf_counter() - self._readiness_snapshot_at) > READINESS_SNAPSHOT_TTL_SECONDS

    def _readiness_evidence(self, report: WindowsSandboxDoctorReport) -> dict[str, Any]:
        return {
            "source": "windows_sandbox_doctor",
            "available": report.available,
            "enforcement_status": report.enforcement_status,
            "blocking_requirements": list(report.blocking_requirements),
            "captured_at_monotonic": self._readiness_snapshot_at,
            "ttl_seconds": READINESS_SNAPSHOT_TTL_SECONDS,
        }

    def _runtime_enforcement_report(
        self,
        prepared: PreparedSandbox,
    ) -> tuple[WindowsSandboxDoctorReport, float]:
        cached = self._readiness_snapshot
        if (
            cached is not None
            and not self._prepared_readiness_requires_recheck(prepared, cached)
        ):
            started = time.perf_counter()
            return cached, time.perf_counter() - started
        return self._readiness_report(refresh=True, force_uncached_default=True)

    def _prepared_readiness_requires_recheck(
        self,
        prepared: PreparedSandbox,
        report: WindowsSandboxDoctorReport,
    ) -> bool:
        if type(self).doctor is not WindowsSandboxBackend.doctor:
            return True
        evidence = prepared.baseline.get("readiness")
        if not isinstance(evidence, dict):
            return True
        if evidence.get("source") != "windows_sandbox_doctor":
            return True
        if evidence.get("available") is not True or not report.available:
            return True
        captured_at = evidence.get("captured_at_monotonic")
        ttl_seconds = evidence.get("ttl_seconds")
        if not isinstance(captured_at, int | float) or not isinstance(ttl_seconds, int | float):
            return True
        if (time.perf_counter() - float(captured_at)) > float(ttl_seconds):
            return True
        return self._network_readiness_needs_recheck(prepared, report)

    @staticmethod
    def _network_readiness_needs_recheck(
        prepared: PreparedSandbox,
        report: WindowsSandboxDoctorReport,
    ) -> bool:
        if prepared.request.profile.network.mode != SandboxNetworkMode.DENIED:
            return False
        role = str(prepared.baseline.get("sandbox_role") or "")
        if role not in {"offline", "online"}:
            return True
        return not (
            report.setup.offline_network_filter.ready
            and _network_probe_state_for_role(report.execution.network_probe, role).ready
        )

    def _fresh_runtime_enforcement_report(self) -> WindowsSandboxDoctorReport:
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
        if not _is_windows():
            return
        run_acl = _apply_run_root_acl(
            sandbox_root,
            account_names=(account_name,),
            operation="run_root_acl",
        )
        if not run_acl.ok:
            raise SandboxCapabilityError(
                "backend_unavailable: sandbox ACL boundary could not be applied."
            )

    def _apply_workspace_low_integrity(self, workspace_root: Path) -> None:
        if not _is_windows():
            return
        icacls = shutil.which("icacls")
        if icacls is None:
            raise SandboxCapabilityError(
                "backend_unavailable: sandbox ACL boundary could not be applied."
            )
        commands = (
            [
                icacls,
                str(workspace_root),
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
