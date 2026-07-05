from __future__ import annotations

import hashlib
import os
import time
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import TYPE_CHECKING, Any

from singularity.command.backend import (
    BackendRunResult,
    ExecutionBackend,
    LocalProcessBackend,
    RunningProcess,
)
from singularity.command.env import EnvPolicy
from singularity.command.models import (
    CommandDecision,
    CommandPlan,
    CommandPolicyResult,
    CommandPurpose,
    CommandRequest,
    CommandResult,
    CommandRisk,
    ExecutionStatus,
    FilesystemMode,
    NetworkMode,
    ProcessOutput,
    ProcessSession,
    ProcessStopResult,
    ResourceLimits,
    SemanticStatus,
)
from singularity.command.output import OutputCollector, OutputSnapshot, SecretRedactor
from singularity.command.policy import CommandPolicy
from singularity.error_codes import ErrorCode
from singularity.kernel.cancellation import throw_if_cancelled
from singularity.observability.models import TraceEventType, TraceSeverity
from singularity.observability.protocols import TraceEmitterProtocol
from singularity.policy import (
    ApprovalGate,
    Capability,
    DecisionOutcome,
    OperationKind,
    PermissionProfileName,
    PolicyComponent,
    PolicyConfig,
    PolicyEngine,
    PolicyError,
    PolicyRequest,
    PolicySubject,
    ResourceRef,
    RiskTag,
)
from singularity.policy.audit import redact, redact_resource_identifier
from singularity.policy.permissions import PermissionProfile, ProtectedPathRule
from singularity.sandbox import (
    SandboxFilesystemMode,
    SandboxManager,
    SandboxNetworkMode,
    SandboxProfileName,
    SandboxRequest,
    SandboxResult,
    SandboxStatus,
    default_sandbox_profile,
)
from singularity.sandbox.models import new_sandbox_id
from singularity.utils.attributes import nested_getattr

if TYPE_CHECKING:
    from singularity.workspace_state import WorkspaceStateManager


SKIP_SIDE_EFFECT_DIRS = {
    ".git",
    ".singularity",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".venv",
    "__pycache__",
    "node_modules",
    "venv",
}

_COMMAND_REDACTION_RULES = [
    "*_TOKEN",
    "*_KEY",
    "*_SECRET",
    "PASSWORD",
    "DATABASE_URL",
    "DSN",
    "*_DSN",
    "CONN_STR",
    "*_CONN_STR",
    "CONN_STRING",
    "*_CONN_STRING",
    "CONNECTION_STRING",
    "*_CONNECTION_STRING",
    "AWS_*",
    "GITHUB_TOKEN",
    "OPENAI_API_KEY",
]


@dataclass(frozen=True)
class WorkspaceSnapshot:
    files: dict[str, str]

    @classmethod
    def capture(cls, workspace_root: Path) -> WorkspaceSnapshot:
        files: dict[str, str] = {}
        if not workspace_root.exists():
            return cls(files)
        for path in sorted(workspace_root.rglob("*")):
            if not path.is_file():
                continue
            try:
                relative = path.relative_to(workspace_root)
            except ValueError:
                continue
            if any(part in SKIP_SIDE_EFFECT_DIRS for part in relative.parts):
                continue
            try:
                files[relative.as_posix()] = hashlib.sha256(path.read_bytes()).hexdigest()
            except OSError:
                continue
        return cls(files)

    def changed_files(self, after: WorkspaceSnapshot) -> list[str]:
        changed = {
            path
            for path, digest in after.files.items()
            if self.files.get(path) != digest
        }
        changed.update(path for path in self.files if path not in after.files)
        return sorted(changed)


@dataclass
class _SessionRecord:
    session: ProcessSession
    running: RunningProcess | None
    before_snapshot: Any
    output_summary: ProcessOutput | None = None


class CommandExecutor:
    def __init__(
        self,
        workspace_root: Path | str,
        *,
        policy: CommandPolicy | None = None,
        backend: ExecutionBackend | None = None,
        trace: TraceEmitterProtocol | None = None,
        env_policy: EnvPolicy | None = None,
        workspace_state_manager: WorkspaceStateManager | None = None,
        planner: Any | None = None,
        policy_engine: PolicyEngine | None = None,
        approval_gate: ApprovalGate | None = None,
        sandbox_manager: SandboxManager | None = None,
    ) -> None:
        self.workspace_root = Path(workspace_root).expanduser().resolve(strict=False)
        self.backend = backend or LocalProcessBackend()
        self.trace = trace
        self.env_policy = env_policy or EnvPolicy()
        self.workspace_state_manager = workspace_state_manager
        self.planner = planner
        self.policy_engine = policy_engine or PolicyEngine(
            PolicyConfig.default_for_workspace(self.workspace_root)
        )
        self.permission_profile = self.policy_engine.config.permission_profile
        self.approval_gate = approval_gate
        self.policy = policy or CommandPolicy()
        self.sandbox_manager = sandbox_manager or SandboxManager(
            self.workspace_root,
            trace=trace if trace is not None and hasattr(trace, "emit") else None,
            permission_profile=self.permission_profile,
        )
        self._sessions: dict[str, _SessionRecord] = {}

    def plan(self, request: CommandRequest) -> CommandPlan:
        policy_request = self._policy_request(request)
        policy_decision = self.policy_engine.enforce(policy_request)
        decision = self._command_policy_result(request, policy_decision)
        cwd = self._resolve_cwd(request.cwd)
        env_result = self.env_policy.build(request.env_request)
        return CommandPlan(
            request=request,
            policy_decision=decision,
            cwd=_relative_or_absolute(cwd, self.workspace_root) if cwd else None,
            backend=self.backend.name,
            env_allowed=sorted(env_result.env),
            env_denied=env_result.denied,
            isolation_report=self._isolation_report(request.resource_limits),
        )

    def run(
        self,
        request: CommandRequest,
        *,
        tool_call_id: str | None = None,
        transaction_id: str | None = None,
    ) -> CommandResult:
        throw_if_cancelled(self)
        self._emit_trace(
            TraceEventType.COMMAND_REQUESTED,
            request,
            summary=f"Command requested: {request.redacted_display_command()}",
            tool_call_id=tool_call_id,
            transaction_id=transaction_id,
        )
        started_at = _now()
        started = time.perf_counter()
        before_snapshot = self._capture_workspace_snapshot()
        git_before = self._git_state_summary()
        policy_request = self._policy_request(request)
        policy_decision = self.policy_engine.enforce(policy_request)
        self._record_policy_trace(policy_request, policy_decision)
        decision = self._command_policy_result(request, policy_decision)
        sandbox_required = (
            policy_decision.outcome == DecisionOutcome.SANDBOX_REQUIRED
            or policy_decision.constraints.sandbox_required
        )
        approved_escalation = False
        approval_grant_id: str | None = None
        if policy_decision.outcome == DecisionOutcome.REQUIRE_REVIEW and self.approval_gate is not None:
            try:
                grant = self.approval_gate.authorize(policy_request, policy_decision)
            except PolicyError:
                grant = None
            if grant is not None:
                approval_grant_id = grant.grant_id
                approved_escalation = True
                sandbox_required = self._permission_profile().profile == PermissionProfileName.READ_ONLY
        if policy_decision.outcome != DecisionOutcome.ALLOW and not sandbox_required:
            if approved_escalation:
                pass
            else:
                result = self._blocked_result(
                    request,
                    decision=decision,
                    started_at=started_at,
                    started=started,
                    git_before=git_before,
                    git_after=git_before,
                )
                self._record_trace(
                    request,
                    result,
                    tool_call_id=tool_call_id,
                    transaction_id=transaction_id,
                )
                self._notify_planner_policy(request, policy_request, policy_decision)
                return result

        cwd = self._resolve_cwd(request.cwd)
        if cwd is None or not cwd.exists() or not cwd.is_dir():
            result = self._cwd_denied_result(
                request,
                decision=decision,
                started_at=started_at,
                started=started,
                git_before=git_before,
                git_after=git_before,
            )
            self._record_trace(
                request,
                result,
                tool_call_id=tool_call_id,
                transaction_id=transaction_id,
            )
            return result

        if sandbox_required:
            env_result = self.env_policy.build(request.env_request)
            sandbox_request = self._sandbox_request(
                request,
                policy_request=policy_request,
                policy_decision=policy_decision,
                cwd=cwd,
            )
            sandbox_result = self.sandbox_manager.run(sandbox_request)
            git_after = self._git_state_summary()
            result = self._result_from_sandbox(
                request,
                decision=decision,
                sandbox_result=sandbox_result,
                redactor=env_result.redactor,
                started_at=started_at,
                started=started,
                git_before=git_before,
                git_after=git_after,
            )
            self._record_trace(
                request,
                result,
                tool_call_id=tool_call_id,
                transaction_id=transaction_id,
            )
            if self.planner is not None:
                self.planner.update_from_command(
                    {"command_result": result.to_observation().get("command_result", result.to_dict())},
                    tool_call_id=tool_call_id,
                )
            self._notify_planner_policy(request, policy_request, policy_decision)
            return result

        env_result = self.env_policy.build(request.env_request)
        collector = self._collector(
            request.command_id,
            request.resource_limits,
            env_result.redactor,
        )
        self._emit_trace(
            TraceEventType.COMMAND_STARTED,
            request,
            summary=f"Command started: {request.redacted_display_command()}",
            tool_call_id=tool_call_id,
            transaction_id=transaction_id,
        )
        backend_result = self._execute_backend(
            request=request,
            cwd=cwd,
            env=env_result.env,
            collector=collector,
        )
        snapshot = collector.snapshot()
        after_snapshot = self._capture_workspace_snapshot()
        side_effects = self._record_command_side_effects(
            request=request,
            before_snapshot=before_snapshot,
            after_snapshot=after_snapshot,
            transaction_id=transaction_id,
        )
        git_after = self._git_state_summary()
        result = self._completed_result(
            request,
            decision=decision,
            backend_result=backend_result,
            output=snapshot,
            changed_files=self._changed_files(before_snapshot, after_snapshot),
            side_effects=side_effects,
            env_denied=env_result.denied,
            started_at=started_at,
            started=started,
            git_before=git_before,
            git_after=git_after,
        )
        self._record_trace(
            request,
            result,
            tool_call_id=tool_call_id,
            transaction_id=transaction_id,
        )
        if self.planner is not None:
            self.planner.update_from_command(
                {"command_result": result.to_observation().get("command_result", result.to_dict())},
                tool_call_id=tool_call_id,
            )
        self._notify_planner_policy(request, policy_request, policy_decision)
        if approved_escalation:
            result.metadata["approved_escalation"] = True
            result.metadata["approval_grant_id"] = approval_grant_id
        return result

    def start_process(
        self,
        request: CommandRequest,
        *,
        tool_call_id: str | None = None,
        transaction_id: str | None = None,
    ) -> ProcessSession:
        throw_if_cancelled(self)
        started_at = _now()
        if self.policy.requires_verification_runner(request):
            return ProcessSession(
                process_id=request.command_id,
                command_id=request.command_id,
                pid=None,
                status="policy_denied",
                argv=request.argv,
                shell=request.shell,
                cwd=request.cwd,
                started_at=started_at,
                owner_transaction=transaction_id,
                error_code=ErrorCode.VERIFICATION_RUNNER_REQUIRED.value,
            )
        policy_request = self._policy_request(request)
        policy_decision = self.policy_engine.enforce(policy_request)
        self._record_policy_trace(policy_request, policy_decision)
        approved_escalation = False
        if policy_decision.outcome == DecisionOutcome.REQUIRE_REVIEW and self.approval_gate:
            try:
                approved_escalation = (
                    self.approval_gate.authorize(policy_request, policy_decision) is not None
                )
            except PolicyError:
                approved_escalation = False
        if (
            policy_decision.outcome == DecisionOutcome.SANDBOX_REQUIRED
            or policy_decision.constraints.sandbox_required
            or (
                approved_escalation
                and self._permission_profile().profile == PermissionProfileName.READ_ONLY
            )
        ):
            return ProcessSession(
                process_id=request.command_id,
                command_id=request.command_id,
                pid=None,
                status="sandbox_required",
                argv=request.argv,
                shell=request.shell,
                cwd=request.cwd,
                started_at=started_at,
                owner_transaction=transaction_id,
                error_code=ErrorCode.SANDBOX_REQUIRED.value,
            )
        if policy_decision.outcome != DecisionOutcome.ALLOW and not approved_escalation:
            return ProcessSession(
                process_id=request.command_id,
                command_id=request.command_id,
                pid=None,
                status=_policy_process_status(policy_decision.outcome),
                argv=request.argv,
                shell=request.shell,
                cwd=request.cwd,
                started_at=started_at,
                owner_transaction=transaction_id,
                error_code=_policy_error_code(policy_decision.outcome),
            )

        cwd = self._resolve_cwd(request.cwd)
        if cwd is None or not cwd.exists() or not cwd.is_dir():
            return ProcessSession(
                process_id=request.command_id,
                command_id=request.command_id,
                pid=None,
                status="policy_denied",
                argv=request.argv,
                shell=request.shell,
                cwd=request.cwd,
                started_at=started_at,
                owner_transaction=transaction_id,
                error_code=ErrorCode.CWD_DENIED.value,
            )

        env_result = self.env_policy.build(request.env_request)
        collector = self._collector(
            request.command_id,
            request.resource_limits,
            env_result.redactor,
        )
        before_snapshot = self._capture_workspace_snapshot()
        running = self.backend.start(
            request=request,
            cwd=cwd,
            env=env_result.env,
            collector=collector,
            owner_transaction=transaction_id,
        )
        status = "running" if running.process is not None else "failed"
        session = ProcessSession(
            process_id=running.process_id,
            command_id=request.command_id,
            pid=running.process.pid if running.process is not None else None,
            status=status,
            argv=request.argv,
            shell=request.shell,
            cwd=_relative_or_absolute(cwd, self.workspace_root),
            started_at=started_at,
            logs_artifact_path=collector.snapshot().artifact_path,
            owner_transaction=transaction_id,
            error_code=running.start_error_code,
        )
        self._sessions[session.process_id] = _SessionRecord(
            session=session,
            running=running,
            before_snapshot=before_snapshot,
        )
        return session

    def read_process_output(self, process_id: str) -> ProcessOutput:
        throw_if_cancelled(self)
        record = self._sessions.get(process_id)
        if record is None:
            return ProcessOutput(
                process_id=process_id,
                stdout="",
                stderr="",
                combined_output="",
                truncated=False,
                artifact_path=None,
            )
        if record.running is None:
            return record.output_summary or ProcessOutput(
                process_id=process_id,
                stdout="",
                stderr="",
                combined_output="",
                truncated=False,
                artifact_path=record.session.logs_artifact_path,
            )
        if isinstance(self.backend, LocalProcessBackend):
            self.backend.poll_output(record.running)
        if record.running.collector is None:
            return record.output_summary or ProcessOutput(
                process_id=process_id,
                stdout="",
                stderr="",
                combined_output="",
                truncated=False,
                artifact_path=record.session.logs_artifact_path,
            )
        snapshot = record.running.collector.read_since_last()
        return ProcessOutput(
            process_id=process_id,
            stdout=snapshot.stdout_preview,
            stderr=snapshot.stderr_preview,
            combined_output=snapshot.combined_output_preview,
            truncated=snapshot.output_truncated,
            artifact_path=snapshot.artifact_path,
        )

    def stop_process(self, process_id: str) -> ProcessStopResult:
        record = self._sessions.get(process_id)
        if record is None:
            return ProcessStopResult(
                process_id=process_id,
                status="not_found",
                exit_code=None,
                killed_reason=None,
                error_code=ErrorCode.PROCESS_NOT_FOUND.value,
            )
        if record.running is None:
            return ProcessStopResult(
                process_id=process_id,
                status=record.session.status,
                exit_code=record.session.exit_code,
                killed_reason="stopped" if record.session.status == "stopped" else None,
                artifact_path=record.session.logs_artifact_path,
                error_code=record.session.error_code,
            )
        exit_code = None
        if isinstance(self.backend, LocalProcessBackend):
            exit_code = self.backend.stop(record.running, reason="stopped")
        after_snapshot = self._capture_workspace_snapshot()
        changed_files = self._changed_files(record.before_snapshot, after_snapshot)
        self._record_command_side_effects(
            request=CommandRequest(
                argv=record.session.argv,
                shell=record.session.shell,
                cwd=record.session.cwd,
                purpose=CommandPurpose.LONG_RUNNING,
                command_id=record.session.command_id,
            ),
            before_snapshot=record.before_snapshot,
            after_snapshot=after_snapshot,
            transaction_id=record.session.owner_transaction,
        )
        snapshot = (
            record.running.collector.snapshot()
            if record.running.collector is not None
            else None
        )
        stopped = ProcessStopResult(
            process_id=process_id,
            status="stopped",
            exit_code=exit_code,
            killed_reason="stopped",
            changed_files=changed_files,
            artifact_path=snapshot.artifact_path if snapshot is not None else None,
        )
        old_session = record.session
        record.session = ProcessSession(
            process_id=old_session.process_id,
            command_id=old_session.command_id,
            pid=old_session.pid,
            status="stopped",
            argv=old_session.argv,
            shell=old_session.shell,
            cwd=old_session.cwd,
            started_at=old_session.started_at,
            ports=old_session.ports,
            health_check=old_session.health_check,
            logs_artifact_path=snapshot.artifact_path if snapshot is not None else None,
            owner_transaction=old_session.owner_transaction,
            exit_code=exit_code,
            error_code=old_session.error_code,
        )
        if snapshot is not None:
            record.output_summary = ProcessOutput(
                process_id=process_id,
                stdout=snapshot.stdout_preview,
                stderr=snapshot.stderr_preview,
                combined_output=snapshot.combined_output_preview,
                truncated=snapshot.output_truncated,
                artifact_path=snapshot.artifact_path,
            )
        record.running = None
        return stopped

    def list_processes(self) -> list[ProcessSession]:
        sessions: list[ProcessSession] = []
        for record in self._sessions.values():
            status = record.session.status
            exit_code = record.session.exit_code
            process = record.running.process if record.running is not None else None
            if process is not None and process.poll() is not None:
                status = "exited"
                exit_code = process.returncode
            sessions.append(
                ProcessSession(
                    process_id=record.session.process_id,
                    command_id=record.session.command_id,
                    pid=record.session.pid,
                    status=status,
                    argv=record.session.argv,
                    shell=record.session.shell,
                    cwd=record.session.cwd,
                    started_at=record.session.started_at,
                    ports=record.session.ports,
                    health_check=record.session.health_check,
                    logs_artifact_path=(
                        record.running.collector.snapshot().artifact_path
                        if record.running is not None and record.running.collector is not None
                        else record.session.logs_artifact_path
                    ),
                    owner_transaction=record.session.owner_transaction,
                    exit_code=exit_code,
                    error_code=record.session.error_code,
                )
            )
        return sessions

    def stop(self) -> None:
        for session in self.list_processes():
            if session.status == "running":
                self.stop_process(session.process_id)

    def _execute_backend(
        self,
        *,
        request: CommandRequest,
        cwd: Path,
        env: dict[str, str],
        collector: OutputCollector,
    ) -> BackendRunResult:
        return self.backend.execute(
            request=request,
            cwd=cwd,
            env=env,
            collector=collector,
            cancellation_token=getattr(self, "cancellation_token", None),
        )

    def _completed_result(
        self,
        request: CommandRequest,
        *,
        decision: CommandPolicyResult,
        backend_result: BackendRunResult,
        output: OutputSnapshot,
        changed_files: list[str],
        side_effects: list[dict[str, Any]],
        env_denied: list[str],
        started_at: str,
        started: float,
        git_before: dict[str, Any],
        git_after: dict[str, Any],
    ) -> CommandResult:
        status = self._execution_status(backend_result)
        semantic_status = self._semantic_status(request, backend_result, status)
        error_code = self._error_code(backend_result, output, semantic_status)
        return CommandResult(
            command_id=request.command_id,
            execution_status=status,
            semantic_status=semantic_status,
            exit_code=backend_result.exit_code,
            signal=backend_result.signal,
            duration_ms=int((time.perf_counter() - started) * 1000),
            timed_out=backend_result.timed_out,
            idle_timed_out=backend_result.idle_timed_out,
            stdout_preview=output.stdout_preview,
            stderr_preview=output.stderr_preview,
            combined_output_preview=output.combined_output_preview,
            output_truncated=output.output_truncated,
            output_digest=output.output_digest,
            artifact_path=output.artifact_path,
            changed_files=changed_files,
            side_effects=side_effects,
            policy_decision=decision,
            risk_tags=decision.risk_tags,
            error_code=error_code,
            isolation_report=self._isolation_report(request.resource_limits),
            env_denied=env_denied,
            killed_reason=backend_result.killed_reason,
            backend=self.backend.name,
            started_at=started_at,
            ended_at=_now(),
            stdout_bytes=output.stdout_bytes,
            stderr_bytes=output.stderr_bytes,
            secret_redactions=output.secret_redactions,
            git_before=git_before,
            git_after=git_after,
            metadata={
                "command_capabilities": self._command_capability_summary(
                    request.resource_limits
                ),
                "sandbox_availability": self._sandbox_availability_summary(None),
            },
        )

    def _result_from_sandbox(
        self,
        request: CommandRequest,
        *,
        decision: CommandPolicyResult,
        sandbox_result: SandboxResult,
        redactor: SecretRedactor,
        started_at: str,
        started: float,
        git_before: dict[str, Any],
        git_after: dict[str, Any],
    ) -> CommandResult:
        execution_status = self._sandbox_execution_status(sandbox_result)
        semantic_status = self._sandbox_semantic_status(
            request,
            sandbox_result,
            execution_status,
        )
        error_code = self._sandbox_error_code(sandbox_result, semantic_status)
        stdout = redactor.redact(sandbox_result.stdout)
        stderr = redactor.redact(sandbox_result.stderr)
        combined = stdout + stderr
        digest = hashlib.sha256(combined.encode("utf-8")).hexdigest()
        artifact_path = (
            sandbox_result.artifacts[0].relative_path
            if sandbox_result.artifacts
            else None
        )
        changed_files = sorted(
            [
                *sandbox_result.filesystem_changes.created_files,
                *sandbox_result.filesystem_changes.modified_files,
                *sandbox_result.filesystem_changes.deleted_files,
            ]
        )
        sandbox_report = {
            "sandbox_id": sandbox_result.sandbox_id,
            "backend": sandbox_result.backend_name,
            "sandbox_backend": sandbox_result.metadata.get("sandbox_backend")
            or sandbox_result.backend_name,
            "status": sandbox_result.status.value,
            "trace_id": sandbox_result.trace_id,
            "enforcement_status": sandbox_result.metadata.get("enforcement_status")
            or (
                "available"
                if sandbox_result.status
                not in {SandboxStatus.BACKEND_UNAVAILABLE, SandboxStatus.SETUP_FAILED}
                else "backend_unavailable"
            ),
            "execution_backend": sandbox_result.metadata.get("execution_backend"),
            "backend_is_local_process": sandbox_result.backend_name == "local_process",
            "sandbox_mode": sandbox_result.metadata.get("sandbox_mode"),
            "sandbox_enforcement": sandbox_result.metadata.get("sandbox_enforcement"),
            "fallback_used": bool(sandbox_result.metadata.get("fallback_used")),
            "fallback_reason": sandbox_result.metadata.get("fallback_reason"),
            "elevated_available": sandbox_result.metadata.get("elevated_available"),
            "elevated_blocker_summary": sandbox_result.metadata.get(
                "elevated_blocker_summary"
            ),
            "used_local_process_fallback": bool(
                sandbox_result.metadata.get("used_local_process_fallback")
            ),
            "local_process_fallback_reason": sandbox_result.metadata.get(
                "local_process_fallback_reason"
            ),
            "network_denied_verified": sandbox_result.metadata.get(
                "network_denied_verified"
            ),
            "network_isolation": sandbox_result.metadata.get("network_isolation"),
            "filesystem_isolation": sandbox_result.metadata.get("filesystem_isolation"),
            "process_tree_kill": sandbox_result.metadata.get("process_tree_kill"),
            "job_killed": sandbox_result.metadata.get("job_killed"),
            "timeout_enforced": sandbox_result.metadata.get("timeout_enforced")
            if sandbox_result.metadata.get("timeout_enforced") is not None
            else sandbox_result.status == SandboxStatus.TIMEOUT,
            "artifact_count": len(sandbox_result.artifacts),
            "artifacts": [artifact.to_dict() for artifact in sandbox_result.artifacts],
            "artifact_refs": [
                artifact.artifact_id for artifact in sandbox_result.artifacts
            ],
            "changed_files": sandbox_result.filesystem_changes.to_dict(),
            "changed_files_count": sandbox_result.filesystem_changes.total_changed_files,
            "violations": [violation.to_dict() for violation in sandbox_result.violations],
            "cleanup_status": sandbox_result.cleanup_status,
            "imported_changes_count": 0,
            "timing": dict(sandbox_result.metadata.get("timing") or {}),
        }
        isolation_report = self._isolation_report(request.resource_limits)
        isolation_report["backend"] = sandbox_result.backend_name
        filesystem_isolation = sandbox_report.get("filesystem_isolation")
        if isinstance(filesystem_isolation, str) and filesystem_isolation:
            isolation_report["filesystem_isolation"] = filesystem_isolation
        elif sandbox_report["sandbox_backend"] == "windows_elevated":
            isolation_report["filesystem_isolation"] = "native_os_sandbox"
        isolation_report["sandbox"] = sandbox_report
        return CommandResult(
            command_id=request.command_id,
            execution_status=execution_status,
            semantic_status=semantic_status,
            exit_code=sandbox_result.exit_code,
            signal=None,
            duration_ms=int((time.perf_counter() - started) * 1000),
            timed_out=sandbox_result.status == SandboxStatus.TIMEOUT,
            idle_timed_out=False,
            stdout_preview=stdout,
            stderr_preview=stderr,
            combined_output_preview=combined,
            output_truncated=bool(sandbox_result.metadata.get("output_truncated")),
            output_digest=digest,
            artifact_path=artifact_path,
            changed_files=changed_files,
            side_effects=[
                {
                    "kind": "sandbox_change_summary",
                    "sandbox_id": sandbox_result.sandbox_id,
                    "backend": sandbox_result.backend_name,
                    "status": sandbox_result.status.value,
                    "changed_files": changed_files,
                    "imported": False,
                }
            ]
            if changed_files
            else [],
            policy_decision=decision,
            risk_tags=decision.risk_tags,
            error_code=error_code,
            isolation_report=isolation_report,
            backend=sandbox_result.backend_name,
            started_at=started_at,
            ended_at=_now(),
            stdout_bytes=len(stdout.encode("utf-8")),
            stderr_bytes=len(stderr.encode("utf-8")),
            git_before=git_before,
            git_after=git_after,
            metadata={
                "sandbox_id": sandbox_result.sandbox_id,
                "sandbox_backend": sandbox_report["sandbox_backend"],
                "sandbox_status": sandbox_result.status.value,
                "sandbox_trace_id": sandbox_result.trace_id,
                "sandbox_artifacts": [artifact.to_dict() for artifact in sandbox_result.artifacts],
                "sandbox_changed_files": sandbox_result.filesystem_changes.to_dict(),
                "sandbox_violations": [violation.to_dict() for violation in sandbox_result.violations],
                "sandbox_timing": dict(sandbox_result.metadata.get("timing") or {}),
                "enforcement_status": sandbox_report["enforcement_status"],
                "execution_backend": sandbox_report["execution_backend"],
                "sandbox_mode": sandbox_report["sandbox_mode"],
                "sandbox_enforcement": sandbox_report["sandbox_enforcement"],
                "fallback_used": sandbox_report["fallback_used"],
                "fallback_reason": sandbox_report["fallback_reason"],
                "elevated_available": sandbox_report["elevated_available"],
                "elevated_blocker_summary": sandbox_report[
                    "elevated_blocker_summary"
                ],
                "used_local_process_fallback": sandbox_report["used_local_process_fallback"],
                "local_process_fallback_reason": sandbox_report[
                    "local_process_fallback_reason"
                ],
                "network_denied_verified": sandbox_report["network_denied_verified"],
                "process_tree_kill": sandbox_report["process_tree_kill"],
                "job_killed": sandbox_report["job_killed"],
                "timeout_enforced": sandbox_report["timeout_enforced"],
                "command_capabilities": self._command_capability_summary(
                    request.resource_limits
                ),
                "sandbox_availability": self._sandbox_availability_summary(
                    sandbox_result.backend_name
                ),
            },
        )

    def _blocked_result(
        self,
        request: CommandRequest,
        *,
        decision: CommandPolicyResult,
        started_at: str,
        started: float,
        git_before: dict[str, Any],
        git_after: dict[str, Any],
    ) -> CommandResult:
        status = (
            ExecutionStatus.REVIEW_REQUIRED
            if decision.decision == CommandDecision.REQUIRE_REVIEW
            else ExecutionStatus.POLICY_DENIED
        )
        return CommandResult(
            command_id=request.command_id,
            execution_status=status,
            semantic_status=SemanticStatus.POLICY_BLOCKED,
            exit_code=None,
            signal=None,
            duration_ms=int((time.perf_counter() - started) * 1000),
            timed_out=False,
            idle_timed_out=False,
            stdout_preview="",
            stderr_preview="",
            combined_output_preview="",
            output_truncated=False,
            output_digest=hashlib.sha256(b"").hexdigest(),
            artifact_path=None,
            changed_files=[],
            policy_decision=decision,
            risk_tags=decision.risk_tags,
            error_code=decision.error_code or ErrorCode.POLICY_DENIED.value,
            isolation_report=self._isolation_report(request.resource_limits),
            backend=self.backend.name,
            started_at=started_at,
            ended_at=_now(),
            git_before=git_before,
            git_after=git_after,
        )

    def _cwd_denied_result(
        self,
        request: CommandRequest,
        *,
        decision: CommandPolicyResult,
        started_at: str,
        started: float,
        git_before: dict[str, Any],
        git_after: dict[str, Any],
    ) -> CommandResult:
        denied = CommandPolicyResult(
            decision=CommandDecision.DENY,
            reasons=[f"cwd does not exist or is not a directory: {request.cwd}"],
            risk_tags=decision.risk_tags,
            required_network=request.network_mode,
            required_filesystem=request.filesystem_mode,
            redaction_rules=decision.redaction_rules,
            error_code=ErrorCode.CWD_DENIED.value,
        )
        return self._blocked_result(
            request,
            decision=denied,
            started_at=started_at,
            started=started,
            git_before=git_before,
            git_after=git_after,
        )

    def _record_trace(
        self,
        request: CommandRequest,
        result: CommandResult,
        *,
        tool_call_id: str | None,
        transaction_id: str | None,
    ) -> None:
        if self.trace is None:
            return
        event_type = TraceEventType.COMMAND_COMPLETED
        severity = TraceSeverity.INFO
        if result.killed_reason:
            event_type = TraceEventType.COMMAND_KILLED
            severity = TraceSeverity.ERROR
        elif result.timed_out or result.idle_timed_out:
            event_type = TraceEventType.COMMAND_TIMEOUT
            severity = TraceSeverity.ERROR
        elif result.semantic_status != SemanticStatus.SUCCEEDED or result.error_code:
            event_type = TraceEventType.COMMAND_FAILED
            severity = TraceSeverity.ERROR
        sandbox = (result.isolation_report or {}).get("sandbox") or {}
        self._emit_trace(
            event_type,
            request,
            summary=result.to_observation()["command_result"]["summary"],
            payload=result.to_observation()["command_result"],
            tool_call_id=tool_call_id,
            transaction_id=transaction_id,
            severity=severity,
            sandbox_id=sandbox.get("sandbox_id"),
            artifact_refs=[result.artifact_path] if result.artifact_path else [],
        )
        self.trace.record(
            "command",
            {
                "command_id": request.command_id,
                "tool_call_id": tool_call_id,
                "transaction_id": transaction_id,
                "command_preview": request.redacted_display_command(),
                "command_hash": request.command_hash(),
                "argv": request.redacted_argv(),
                "shell": request.redacted_shell(),
                "cwd": redact_resource_identifier(request.cwd),
                "backend": result.backend,
                "sandbox_id": sandbox.get("sandbox_id"),
                "policy_decision": result.policy_decision.decision.value,
                "policy_reasons": result.policy_decision.reasons,
                "risk_tags": [tag.value for tag in result.risk_tags],
                "env_policy": {
                    "inherit_parent_env": False,
                    "env_denied": result.env_denied,
                    "redaction_rules": result.policy_decision.redaction_rules,
                },
                "network_mode": request.network_mode.value,
                "filesystem_mode": request.filesystem_mode.value,
                "resource_limits": request.resource_limits.to_dict(),
                "started_at": result.started_at,
                "ended_at": result.ended_at,
                "duration_ms": result.duration_ms,
                "exit_code": result.exit_code,
                "signal": result.signal,
                "stdout_bytes": result.stdout_bytes,
                "stderr_bytes": result.stderr_bytes,
                "output_digest": result.output_digest,
                "artifact_ref": result.artifact_path,
                "changed_files": result.changed_files,
                "side_effects": result.side_effects,
                "secret_redactions": result.secret_redactions,
                "error_code": result.error_code,
                "semantic_status": result.semantic_status.value,
                "isolation_report": result.isolation_report,
                "git_before": result.git_before,
                "git_after": result.git_after,
                "policy_engine": True,
            },
        )

    def _emit_trace(
        self,
        event_type: TraceEventType,
        request: CommandRequest,
        *,
        summary: str,
        payload: dict[str, Any] | None = None,
        tool_call_id: str | None = None,
        transaction_id: str | None = None,
        severity: TraceSeverity = TraceSeverity.INFO,
        sandbox_id: str | None = None,
        artifact_refs: list[str] | None = None,
    ) -> None:
        if self.trace is None or not hasattr(self.trace, "emit"):
            return
        self.trace.emit(
            event_type,
            component="command",
            summary=summary,
            payload=payload
            or {
                "command_preview": request.redacted_display_command(),
                "command_hash": request.command_hash(),
                "cwd": redact_resource_identifier(request.cwd),
                "purpose": request.purpose.value,
                "network_mode": request.network_mode.value,
                "filesystem_mode": request.filesystem_mode.value,
            },
            ids={
                "session_id": getattr(self.planner, "session_id", None),
                "task_id": getattr(self.planner, "task_id", None),
                "phase_id": nested_getattr(self.planner, "state.current_phase"),
                "action_id": tool_call_id or request.command_id,
                "command_id": request.command_id,
                "transaction_id": transaction_id,
                "sandbox_id": sandbox_id,
            },
            severity=severity,
            artifact_refs=artifact_refs,
        )

    def _collector(
        self,
        command_id: str,
        limits: ResourceLimits,
        redactor: SecretRedactor,
    ) -> OutputCollector:
        return OutputCollector(
            workspace_root=self.workspace_root,
            command_id=command_id,
            limits=limits,
            redactor=redactor,
        )

    def _capture_workspace_snapshot(self) -> Any:
        if self.workspace_state_manager is not None:
            return self.workspace_state_manager.capture_snapshot()
        return WorkspaceSnapshot.capture(self.workspace_root)

    def _changed_files(self, before: Any, after: Any) -> list[str]:
        if isinstance(before, WorkspaceSnapshot) and isinstance(after, WorkspaceSnapshot):
            return before.changed_files(after)
        before_hashes = {
            path: snapshot.sha256 for path, snapshot in before.items()
        }
        after_hashes = {
            path: snapshot.sha256 for path, snapshot in after.items()
        }
        changed = {
            path for path, digest in after_hashes.items() if before_hashes.get(path) != digest
        }
        changed.update(path for path in before_hashes if path not in after_hashes)
        return sorted(changed)

    def _record_command_side_effects(
        self,
        *,
        request: CommandRequest,
        before_snapshot: Any,
        after_snapshot: Any,
        transaction_id: str | None,
    ) -> list[dict[str, Any]]:
        if self.workspace_state_manager is None:
            return []
        changes = self.workspace_state_manager.record_command_side_effects(
            command_id=request.command_id,
            purpose=request.purpose,
            before_snapshot=before_snapshot,
            after_snapshot=after_snapshot,
            transaction_id=transaction_id,
        )
        return [change.to_dict() for change in changes]

    def _resolve_cwd(self, cwd: str) -> Path | None:
        root = self.workspace_root
        raw = Path(cwd)
        candidate = raw if raw.is_absolute() else root / raw
        try:
            resolved = candidate.resolve(strict=False)
            root_key = os.path.normcase(os.path.normpath(str(root)))
            candidate_key = os.path.normcase(os.path.normpath(str(resolved)))
            if os.path.commonpath([root_key, candidate_key]) != root_key:
                return None
            return resolved
        except (OSError, ValueError):
            return None

    @staticmethod
    def _execution_status(backend_result: BackendRunResult) -> ExecutionStatus:
        if backend_result.error_code in {
            ErrorCode.COMMAND_NOT_FOUND.value,
            ErrorCode.SPAWN_FAILED.value,
            ErrorCode.PERMISSION_ERROR.value,
        }:
            return ExecutionStatus.SPAWN_FAILED
        if backend_result.error_code:
            return ExecutionStatus.BACKEND_ERROR
        if backend_result.timed_out:
            return ExecutionStatus.TIMED_OUT
        if backend_result.idle_timed_out:
            return ExecutionStatus.IDLE_TIMED_OUT
        return ExecutionStatus.COMPLETED

    @staticmethod
    def _sandbox_execution_status(sandbox_result: SandboxResult) -> ExecutionStatus:
        if sandbox_result.status == SandboxStatus.TIMEOUT:
            return ExecutionStatus.TIMED_OUT
        if sandbox_result.status in {
            SandboxStatus.BACKEND_UNAVAILABLE,
            SandboxStatus.SETUP_FAILED,
            SandboxStatus.CLEANUP_FAILED,
        }:
            return ExecutionStatus.BACKEND_ERROR
        if sandbox_result.status in {SandboxStatus.POLICY_BLOCKED, SandboxStatus.VIOLATION}:
            return ExecutionStatus.POLICY_DENIED
        return ExecutionStatus.COMPLETED

    @staticmethod
    def _semantic_status(
        request: CommandRequest,
        backend_result: BackendRunResult,
        execution_status: ExecutionStatus,
    ) -> SemanticStatus:
        if execution_status != ExecutionStatus.COMPLETED:
            return SemanticStatus.EXECUTION_FAILED
        if backend_result.exit_code == 0:
            return SemanticStatus.SUCCEEDED
        if request.purpose == CommandPurpose.PROJECT_VERIFICATION:
            return SemanticStatus.TESTS_FAILED
        if request.purpose == CommandPurpose.BUILD:
            return SemanticStatus.BUILD_FAILED
        if request.purpose == CommandPurpose.LINT:
            return SemanticStatus.LINT_FAILED
        if request.purpose == CommandPurpose.TYPECHECK:
            return SemanticStatus.TYPECHECK_FAILED
        if request.purpose == CommandPurpose.FORMAT_CHECK:
            return SemanticStatus.LINT_FAILED
        if request.purpose == CommandPurpose.FORMATTER:
            return SemanticStatus.LINT_FAILED
        return SemanticStatus.EXIT_NONZERO

    @staticmethod
    def _sandbox_semantic_status(
        request: CommandRequest,
        sandbox_result: SandboxResult,
        execution_status: ExecutionStatus,
    ) -> SemanticStatus:
        if execution_status != ExecutionStatus.COMPLETED:
            return SemanticStatus.EXECUTION_FAILED
        if sandbox_result.exit_code == 0:
            return SemanticStatus.SUCCEEDED
        if request.purpose == CommandPurpose.PROJECT_VERIFICATION:
            return SemanticStatus.TESTS_FAILED
        if request.purpose == CommandPurpose.BUILD:
            return SemanticStatus.BUILD_FAILED
        if request.purpose == CommandPurpose.LINT:
            return SemanticStatus.LINT_FAILED
        if request.purpose == CommandPurpose.TYPECHECK:
            return SemanticStatus.TYPECHECK_FAILED
        if request.purpose in {CommandPurpose.FORMAT_CHECK, CommandPurpose.FORMATTER}:
            return SemanticStatus.LINT_FAILED
        return SemanticStatus.EXIT_NONZERO

    @staticmethod
    def _error_code(
        backend_result: BackendRunResult,
        output: OutputSnapshot,
        semantic_status: SemanticStatus,
    ) -> str | None:
        if backend_result.error_code:
            return backend_result.error_code
        if backend_result.timed_out:
            return ErrorCode.TIMEOUT.value
        if backend_result.idle_timed_out:
            return ErrorCode.IDLE_TIMEOUT.value
        if semantic_status in {
            SemanticStatus.TESTS_FAILED,
            SemanticStatus.BUILD_FAILED,
            SemanticStatus.LINT_FAILED,
            SemanticStatus.TYPECHECK_FAILED,
        }:
            return ErrorCode.SEMANTIC_FAILURE.value
        if semantic_status == SemanticStatus.EXIT_NONZERO:
            return ErrorCode.EXIT_NONZERO.value
        if output.output_truncated:
            return ErrorCode.OUTPUT_LIMIT_EXCEEDED.value
        return None

    @staticmethod
    def _sandbox_error_code(
        sandbox_result: SandboxResult,
        semantic_status: SemanticStatus,
    ) -> str | None:
        if sandbox_result.status == SandboxStatus.BACKEND_UNAVAILABLE:
            return ErrorCode.SANDBOX_UNAVAILABLE.value
        if sandbox_result.status == SandboxStatus.VIOLATION:
            return ErrorCode.SANDBOX_VIOLATION.value
        if sandbox_result.status == SandboxStatus.TIMEOUT:
            return ErrorCode.TIMEOUT.value
        if sandbox_result.metadata.get("error_code"):
            return str(sandbox_result.metadata["error_code"])
        if semantic_status in {
            SemanticStatus.TESTS_FAILED,
            SemanticStatus.BUILD_FAILED,
            SemanticStatus.LINT_FAILED,
            SemanticStatus.TYPECHECK_FAILED,
        }:
            return ErrorCode.SEMANTIC_FAILURE.value
        if semantic_status == SemanticStatus.EXIT_NONZERO:
            return ErrorCode.EXIT_NONZERO.value
        if sandbox_result.metadata.get("output_truncated"):
            return ErrorCode.OUTPUT_LIMIT_EXCEEDED.value
        return None

    @staticmethod
    def _isolation_report(limits: ResourceLimits) -> dict[str, Any]:
        unsupported = []
        if limits.max_memory_mb is not None:
            unsupported.append("max_memory_mb")
        if limits.max_processes is not None:
            unsupported.append("max_processes")
        if limits.max_disk_write_mb is not None:
            unsupported.append("max_disk_write_mb")
        return {
            "backend": "local_process",
            "network_isolation_enforced": False,
            "filesystem_isolation": "workspace_cwd_advisory",
            "home_access_blocked": False,
            "resource_limits_enforced": [
                "timeout_seconds",
                "idle_timeout_seconds",
                "max_stdout_bytes",
                "max_stderr_bytes",
                "max_combined_output_bytes",
            ],
            "resource_limits_unsupported": unsupported,
        }

    def _command_capability_summary(self, limits: ResourceLimits) -> dict[str, Any]:
        isolation = self._isolation_report(limits)
        return {
            "backend": self.backend.name,
            "timeout": True,
            "idle_timeout": True,
            "output_limit": True,
            "process_tree_kill": True,
            "network_mode": isolation["network_isolation_enforced"],
            "filesystem_mode": isolation["filesystem_isolation"],
        }

    def _sandbox_availability_summary(
        self,
        selected_backend: str | None,
    ) -> dict[str, Any]:
        backends = list(getattr(self.sandbox_manager, "backends", []) or [])
        available = [backend for backend in backends if _sandbox_backend_available(backend)]
        capabilities = [backend.capabilities() for backend in available]
        return {
            "registered_backends": [backend.name() for backend in backends],
            "available_backends": [backend.name() for backend in available],
            "selected_backend": selected_backend,
            "hard_isolation_available": any(item.network_isolation for item in capabilities),
            "network_isolation_available": any(item.network_isolation for item in capabilities),
            "memory_limit_available": any(item.memory_limit for item in capabilities),
            "process_limit_available": any(item.process_limit for item in capabilities),
        }

    def _git_state_summary(self) -> dict[str, Any]:
        git_dir = self.workspace_root / ".git"
        if not git_dir.exists():
            return {"available": False, "reason": "not_a_git_worktree"}
        head_file = git_dir / "HEAD"
        try:
            head_text = head_file.read_text(encoding="utf-8").strip()
        except OSError as exc:
            return {"available": False, "reason": str(exc)}

        branch: str | None = None
        head: str | None = head_text
        if head_text.startswith("ref: "):
            ref = head_text.removeprefix("ref: ").strip()
            branch = ref.removeprefix("refs/heads/")
            ref_path = git_dir / ref
            try:
                head = ref_path.read_text(encoding="utf-8").strip()
            except OSError:
                head = None
        return {
            "available": True,
            "collector": "lightweight_filesystem",
            "branch": branch,
            "head": head,
        }

    def _policy_request(self, request: CommandRequest) -> PolicyRequest:
        operation, capability, resource = _command_policy_shape(request)
        risk_tags = self.policy.classify(request)
        cwd_outside_workspace = _cwd_outside_workspace(self.workspace_root, request.cwd)
        return PolicyRequest(
            session_id=getattr(self.planner, "session_id", "command_session"),
            task_id=getattr(self.planner, "task_id", "command_task"),
            phase_id=nested_getattr(self.planner, "state.current_phase", default="command"),
            action_id=request.command_id,
            component=PolicyComponent.COMMAND,
            operation=operation,
            capability=capability,
            subject=PolicySubject(subject_type="component", name="CommandExecutor"),
            resource=resource,
            reason=request.redacted_display_command(),
            proposed_by_model=True,
            risk_tags=_policy_risk_tags(request, risk_tags),
            metadata={
                **request.safe_metadata(),
                "env_policy": request.env_request,
                "command_purpose": request.purpose.value,
                "command_risk_tags": [tag.value for tag in risk_tags],
                "command_missing": not bool(request.argv or request.shell),
                "cwd_outside_workspace": cwd_outside_workspace,
            },
            requires_network=request.network_mode != NetworkMode.DISABLED,
            touches_workspace=request.filesystem_mode != FilesystemMode.READ_ONLY_WORKSPACE,
            long_running=request.purpose == CommandPurpose.LONG_RUNNING,
            destructive=request.purpose == CommandPurpose.DESTRUCTIVE,
            workspace_root=str(self.workspace_root),
        )

    def _command_policy_result(
        self,
        request: CommandRequest,
        decision: Any,
    ) -> CommandPolicyResult:
        command_decision = (
            CommandDecision.ALLOW
            if decision.outcome in {DecisionOutcome.ALLOW, DecisionOutcome.SANDBOX_REQUIRED}
            else CommandDecision.REQUIRE_REVIEW
            if decision.outcome == DecisionOutcome.REQUIRE_REVIEW
            else CommandDecision.DENY
        )
        error_code = None if command_decision == CommandDecision.ALLOW else _policy_error_code(decision.outcome)
        if decision.outcome == DecisionOutcome.DENY and decision.rule_ids:
            if "hard_deny_cwd_outside_workspace" in decision.rule_ids:
                error_code = "cwd_outside_workspace"
            elif any("protected_path" in rule_id for rule_id in decision.rule_ids):
                error_code = ErrorCode.PROTECTED_PATH_DENIED.value
        return CommandPolicyResult(
            decision=command_decision,
            reasons=[decision.reason],
            risk_tags=self.policy.classify(request),
            required_backend=(
                "sandbox"
                if decision.outcome == DecisionOutcome.SANDBOX_REQUIRED
                or decision.constraints.sandbox_required
                else self.backend.name
            ),
            required_network=request.network_mode,
            required_filesystem=request.filesystem_mode,
            redaction_rules=_COMMAND_REDACTION_RULES,
            error_code=error_code,
        )

    def _sandbox_request(
        self,
        request: CommandRequest,
        *,
        policy_request: PolicyRequest,
        policy_decision: Any,
        cwd: Path,
    ) -> SandboxRequest:
        permission_profile = self._permission_profile()
        profile_name = (
            SandboxProfileName.READONLY_ANALYSIS
            if permission_profile.profile == PermissionProfileName.READ_ONLY
            else SandboxProfileName.ISOLATED_VERIFICATION
        )
        profile = default_sandbox_profile(profile_name, workspace_root=self.workspace_root)
        if permission_profile.profile != PermissionProfileName.READ_ONLY:
            profile.filesystem.mode = SandboxFilesystemMode.COPY_ON_WRITE_WORKSPACE
            profile.filesystem.writable_paths = [
                str(path)
                for path in (
                    *permission_profile.workspace_roots,
                    *permission_profile.additional_writable_directories,
                )
            ]
        profile.filesystem.exclude_globs = sorted(
            {
                *profile.filesystem.exclude_globs,
                *self._protected_path_patterns(permission_profile),
            }
        )
        profile.network.mode = SandboxNetworkMode(
            permission_profile.network_access.value
        )
        profile.network.require_hard_isolation = (
            profile.network.mode == SandboxNetworkMode.DENIED
        )
        profile.resources.timeout_seconds = int(request.resource_limits.timeout_seconds)
        profile.resources.max_output_chars = request.resource_limits.max_combined_output_bytes
        env_result = self.env_policy.build(request.env_request)
        profile.env.extra_env = dict(env_result.env)
        command: list[str] | str = request.argv or request.shell or ""
        return SandboxRequest(
            sandbox_id=new_sandbox_id(),
            session_id=policy_request.session_id,
            task_id=policy_request.task_id,
            action_id=policy_request.action_id,
            command=command,
            cwd=cwd,
            workspace_root=self.workspace_root,
            profile=profile,
            policy_decision_id=policy_decision.decision_id,
            policy_constraints=policy_decision.constraints,
            reason=policy_decision.reason,
            metadata={
                "command_id": request.command_id,
                "permission_profile": permission_profile.profile.value,
            },
        )

    def _permission_profile(self) -> PermissionProfile:
        if self.permission_profile is None:
            raise RuntimeError("policy engine must provide a permission profile")
        return self.permission_profile

    @staticmethod
    def _protected_path_patterns(permission_profile: PermissionProfile) -> list[str]:
        return [
            rule.pattern if isinstance(rule, ProtectedPathRule) else str(rule)
            for rule in permission_profile.protected_paths
        ]

    def _notify_planner_policy(
        self,
        request: CommandRequest,
        policy_request: PolicyRequest,
        decision: Any,
    ) -> None:
        if self.planner is None or not hasattr(self.planner, "record_policy_observation"):
            return
        self.planner.record_policy_observation(
            {
                "outcome": decision.outcome.value,
                "component": policy_request.component.value,
                "operation": policy_request.operation.value,
                "reason": redact(decision.reason),
                "risk_level": decision.risk_level.value,
                "resource": redact_resource_identifier(policy_request.resource.identifier),
                "decision_id": decision.decision_id,
            }
        )

    def _record_policy_trace(self, request: PolicyRequest, decision: Any) -> None:
        if self.trace is None:
            return
        self.trace.record(
            "policy",
            redact(
                {
                    "request_id": request.request_id,
                    "decision_id": decision.decision_id,
                    "component": request.component.value,
                    "operation": request.operation.value,
                    "capability": request.capability.value,
                    "resource": redact_resource_identifier(request.resource.identifier),
                    "outcome": decision.outcome.value,
                    "risk_level": decision.risk_level.value,
                    "risk_tags": [
                        tag.value if hasattr(tag, "value") else str(tag)
                        for tag in decision.risk_tags
                    ],
                    "reason": decision.reason,
                    "rule_ids": decision.rule_ids,
                    "approval_required": decision.required_approval is not None,
                }
            ),
        )


def _now() -> str:
    return datetime.now(UTC).isoformat()


def _relative_or_absolute(path: Path, root: Path) -> str:
    try:
        return path.relative_to(root).as_posix() or "."
    except ValueError:
        return str(path)


def _sandbox_backend_available(backend: Any) -> bool:
    if not hasattr(backend, "is_available"):
        return True
    try:
        return bool(backend.is_available())
    except Exception:
        return False


def _command_policy_shape(
    request: CommandRequest,
) -> tuple[OperationKind, Capability, ResourceRef]:
    command = request.redacted_display_command()
    if request.purpose == CommandPurpose.PACKAGE_MANAGER and _looks_like_package_manager(request):
        return (
            OperationKind.PACKAGE_INSTALL,
            Capability.PACKAGE_INSTALL,
            ResourceRef("command", command),
        )
    if request.purpose == CommandPurpose.NETWORK or request.network_mode != NetworkMode.DISABLED:
        return (
            OperationKind.NETWORK_ACCESS,
            Capability.NETWORK_ACCESS,
            ResourceRef("command", command),
        )
    if request.purpose == CommandPurpose.LONG_RUNNING:
        return (
            OperationKind.START_LONG_PROCESS,
            Capability.START_LONG_PROCESS,
            ResourceRef("command", command),
        )
    if request.purpose in {
        CommandPurpose.PROJECT_VERIFICATION,
        CommandPurpose.LINT,
        CommandPurpose.TYPECHECK,
        CommandPurpose.FORMAT_CHECK,
        CommandPurpose.BUILD,
    }:
        return (
            OperationKind.VERIFICATION,
            Capability.EXECUTE_PROJECT_CODE,
            ResourceRef("command", command),
        )
    if request.purpose == CommandPurpose.CODE_GENERATION:
        return (
            OperationKind.EXECUTE_PROJECT_CODE,
            Capability.EXECUTE_GENERATED_CODE,
            ResourceRef("command", command),
        )
    return (
        OperationKind.EXECUTE_COMMAND,
        Capability.EXECUTE_COMMAND,
        ResourceRef("command", command),
    )


def _looks_like_package_manager(request: CommandRequest) -> bool:
    argv = [str(part).lower() for part in (request.argv or [])]
    if not argv:
        return bool(request.shell and any(token in request.shell.lower() for token in ("npm install", "pnpm install", "yarn add", "pip install", "uv pip install", "cargo install")))
    program = Path(argv[0]).name.lower()
    for suffix in (".exe", ".cmd", ".bat"):
        if program.endswith(suffix):
            program = program[: -len(suffix)]
    if program in {"npm", "pnpm", "yarn", "cargo"}:
        return any(part in {"install", "add", "update", "upgrade"} for part in argv[1:])
    if program in {"python", "python3", "py"}:
        return argv[1:3] == ["-m", "pip"] and any(part in {"install", "uninstall"} for part in argv[3:])
    return program == "uv" and any(part in {"add", "pip", "sync"} for part in argv[1:])


def _cwd_outside_workspace(workspace_root: Path, cwd: str) -> bool:
    root = workspace_root.expanduser().resolve(strict=False)
    raw = Path(cwd)
    candidate = raw if raw.is_absolute() else root / raw
    try:
        resolved = candidate.resolve(strict=False)
        root_key = os.path.normcase(os.path.normpath(str(root)))
        candidate_key = os.path.normcase(os.path.normpath(str(resolved)))
        return os.path.commonpath([root_key, candidate_key]) != root_key
    except (OSError, ValueError):
        return True


def _policy_risk_tags(
    request: CommandRequest,
    command_risks: list[CommandRisk],
) -> list[RiskTag]:
    tags: set[RiskTag] = set()
    for risk in command_risks:
        if risk == CommandRisk.NETWORK:
            tags.add(RiskTag.NETWORK)
        elif risk == CommandRisk.WRITE_WORKSPACE:
            tags.add(RiskTag.MUTATES_FILES)
        elif risk == CommandRisk.DESTRUCTIVE:
            tags.update({RiskTag.DESTRUCTIVE, RiskTag.IRREVERSIBLE, RiskTag.MUTATES_FILES})
        elif risk == CommandRisk.PACKAGE_MANAGER:
            tags.update({RiskTag.PACKAGE_MANAGER, RiskTag.SUPPLY_CHAIN, RiskTag.MUTATES_FILES})
        elif risk == CommandRisk.LONG_RUNNING:
            tags.add(RiskTag.LONG_RUNNING)
        elif risk in {CommandRisk.PROJECT_VERIFICATION, CommandRisk.EXECUTES_PROJECT_CODE}:
            tags.add(RiskTag.EXECUTES_PROJECT_CODE)
        elif risk == CommandRisk.CODE_GENERATION:
            tags.add(RiskTag.EXECUTES_GENERATED_CODE)
    if request.shell is not None:
        tags.add(RiskTag.SHELL_EXPANSION)
    return sorted(tags, key=lambda tag: tag.value)


def _policy_error_code(outcome: DecisionOutcome) -> str:
    mapping = {
        DecisionOutcome.DENY: ErrorCode.POLICY_DENIED.value,
        DecisionOutcome.REQUIRE_REVIEW: ErrorCode.REVIEW_REQUIRED.value,
        DecisionOutcome.SANDBOX_REQUIRED: ErrorCode.SANDBOX_REQUIRED.value,
        DecisionOutcome.ASK_USER: ErrorCode.POLICY_ASK_USER_REQUIRED.value,
        DecisionOutcome.ESCALATE: ErrorCode.POLICY_ESCALATION_REQUIRED.value,
    }
    return mapping.get(outcome, ErrorCode.POLICY_DENIED.value)


def _policy_process_status(outcome: DecisionOutcome) -> str:
    if outcome == DecisionOutcome.REQUIRE_REVIEW:
        return ErrorCode.REVIEW_REQUIRED.value
    return _policy_error_code(outcome)
