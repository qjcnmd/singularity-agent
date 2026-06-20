from __future__ import annotations

import hashlib
import os
import time
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import TYPE_CHECKING, Any

from miniharness.command.backend import (
    BackendRunResult,
    ExecutionBackend,
    LocalProcessBackend,
    RunningProcess,
)
from miniharness.command.env import EnvPolicy
from miniharness.command.models import (
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
from miniharness.command.output import OutputCollector, OutputSnapshot, SecretRedactor
from miniharness.command.policy import CommandPolicy
from miniharness.observability.models import TraceEventType, TraceSeverity
from miniharness.trace import TraceWriter
from miniharness.policy import (
    Capability,
    DecisionOutcome,
    OperationKind,
    PolicyConfig,
    PolicyRequest,
    PolicyRuntime,
    PolicySubject,
    ResourceRef,
    RuntimeName,
)
from miniharness.policy.audit import redact, redact_resource_identifier
from miniharness.sandbox import (
    SandboxResult,
    SandboxRuntime,
    SandboxStatus,
)

if TYPE_CHECKING:
    from miniharness.workspace_state import LocalWorkspaceStateRuntime


SKIP_SIDE_EFFECT_DIRS = {
    ".git",
    ".miniharness",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".venv",
    "__pycache__",
    "node_modules",
    "venv",
}


@dataclass(frozen=True)
class WorkspaceSnapshot:
    files: dict[str, str]

    @classmethod
    def capture(cls, workspace_root: Path) -> "WorkspaceSnapshot":
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

    def changed_files(self, after: "WorkspaceSnapshot") -> list[str]:
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
    running: RunningProcess
    before_snapshot: Any


class CommandRuntime:
    def __init__(
        self,
        workspace_root: Path | str,
        *,
        policy: CommandPolicy | None = None,
        backend: ExecutionBackend | None = None,
        trace: TraceWriter | None = None,
        env_policy: EnvPolicy | None = None,
        state_runtime: "LocalWorkspaceStateRuntime | None" = None,
        planner: Any | None = None,
        policy_runtime: PolicyRuntime | None = None,
        sandbox_runtime: SandboxRuntime | None = None,
    ) -> None:
        self.workspace_root = Path(workspace_root).expanduser().resolve(strict=False)
        self.backend = backend or LocalProcessBackend()
        self.trace = trace
        self.env_policy = env_policy or EnvPolicy()
        self.state_runtime = state_runtime
        self.planner = planner
        self.policy_runtime = policy_runtime or PolicyRuntime(
            PolicyConfig.runtime_default(self.workspace_root)
        )
        self.policy = policy or CommandPolicy(
            security_mode=self.policy_runtime.config.security_mode
        )
        self.sandbox_runtime = sandbox_runtime or SandboxRuntime(
            self.workspace_root,
            trace=trace if trace is not None and hasattr(trace, "emit") else None,
            security_mode=self.policy_runtime.config.security_mode,
        )
        self._sessions: dict[str, _SessionRecord] = {}

    def plan(self, request: CommandRequest) -> CommandPlan:
        decision = self.policy.evaluate(request, workspace_root=self.workspace_root)
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
        self._throw_if_cancelled()
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
        policy_decision = self.policy_runtime.enforce(policy_request)
        self._record_policy_trace(policy_request, policy_decision)
        sandbox_required = (
            policy_decision.outcome == DecisionOutcome.SANDBOX_REQUIRED
            or policy_decision.constraints.sandbox_required
        )
        if policy_decision.outcome != DecisionOutcome.ALLOW and not sandbox_required:
            result = self._policy_blocked_result(
                request,
                decision=policy_decision,
                started_at=started_at,
                started=started,
                git_before=git_before,
                git_after=git_before,
                policy_request=policy_request,
            )
            self._record_trace(
                request,
                result,
                tool_call_id=tool_call_id,
                transaction_id=transaction_id,
            )
            self._notify_planner_policy(request, policy_request, policy_decision)
            return result
        decision = self.policy.evaluate(request, workspace_root=self.workspace_root)
        if decision.decision == CommandDecision.DENY or (
            decision.decision != CommandDecision.ALLOW and not sandbox_required
        ):
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
            sandbox_request = self.sandbox_runtime.build_request_from_policy(
                request,
                policy_decision,
                session_id=policy_request.session_id,
                task_id=policy_request.task_id,
                action_id=policy_request.action_id,
                cwd=cwd,
            )
            sandbox_result = self.sandbox_runtime.run(sandbox_request)
            git_after = self._git_state_summary()
            result = self._result_from_sandbox(
                request,
                decision=decision,
                sandbox_result=sandbox_result,
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
        return result

    def start_process(
        self,
        request: CommandRequest,
        *,
        tool_call_id: str | None = None,
        transaction_id: str | None = None,
    ) -> ProcessSession:
        self._throw_if_cancelled()
        started_at = _now()
        if self.policy.requires_verification_runtime(request):
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
                error_code="verification_runtime_required",
            )
        decision = self.policy.evaluate(request, workspace_root=self.workspace_root)
        policy_request = self._policy_request(request)
        policy_decision = self.policy_runtime.enforce(policy_request)
        self._record_policy_trace(policy_request, policy_decision)
        if (
            policy_decision.outcome == DecisionOutcome.SANDBOX_REQUIRED
            or policy_decision.constraints.sandbox_required
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
                error_code="sandbox_required",
            )
        if policy_decision.outcome != DecisionOutcome.ALLOW:
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
        decision = self.policy.evaluate(request, workspace_root=self.workspace_root)
        if decision.decision != CommandDecision.ALLOW:
            session = ProcessSession(
                process_id=request.command_id,
                command_id=request.command_id,
                pid=None,
                status=decision.decision.value,
                argv=request.argv,
                shell=request.shell,
                cwd=request.cwd,
                started_at=started_at,
                owner_transaction=transaction_id,
                error_code=decision.error_code,
            )
            return session

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
                error_code="cwd_denied",
            )

        env_result = self.env_policy.build(request.env_request)
        collector = self._collector(
            request.command_id,
            request.resource_limits,
            env_result.redactor,
        )
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
            before_snapshot=self._capture_workspace_snapshot(),
        )
        return session

    def read_process_output(self, process_id: str) -> ProcessOutput:
        self._throw_if_cancelled()
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
        if isinstance(self.backend, LocalProcessBackend):
            self.backend.poll_output(record.running)
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
                error_code="process_not_found",
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
        snapshot = record.running.collector.snapshot()
        stopped = ProcessStopResult(
            process_id=process_id,
            status="stopped",
            exit_code=exit_code,
            killed_reason="stopped",
            changed_files=changed_files,
            artifact_path=snapshot.artifact_path,
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
            logs_artifact_path=snapshot.artifact_path,
            owner_transaction=old_session.owner_transaction,
            exit_code=exit_code,
            error_code=old_session.error_code,
        )
        return stopped

    def list_processes(self) -> list[ProcessSession]:
        sessions: list[ProcessSession] = []
        for record in self._sessions.values():
            process = record.running.process
            status = record.session.status
            exit_code = record.session.exit_code
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
                    logs_artifact_path=record.running.collector.snapshot().artifact_path,
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
        try:
            return self.backend.execute(
                request=request,
                cwd=cwd,
                env=env,
                collector=collector,
                cancellation_token=getattr(self, "cancellation_token", None),
            )
        except TypeError as exc:
            if "cancellation_token" not in str(exc):
                raise
            return self.backend.execute(
                request=request,
                cwd=cwd,
                env=env,
                collector=collector,
            )

    def _throw_if_cancelled(self) -> None:
        token = getattr(self, "cancellation_token", None)
        if token is not None and hasattr(token, "throw_if_cancelled"):
            token.throw_if_cancelled()

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
        )

    def _result_from_sandbox(
        self,
        request: CommandRequest,
        *,
        decision: CommandPolicyResult,
        sandbox_result: SandboxResult,
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
        stdout = sandbox_result.stdout
        stderr = sandbox_result.stderr
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
            "status": sandbox_result.status.value,
            "trace_id": sandbox_result.trace_id,
            "artifact_count": len(sandbox_result.artifacts),
            "artifacts": [artifact.to_dict() for artifact in sandbox_result.artifacts],
            "changed_files": sandbox_result.filesystem_changes.to_dict(),
            "changed_files_count": sandbox_result.filesystem_changes.total_changed_files,
            "violations": [violation.to_dict() for violation in sandbox_result.violations],
            "cleanup_status": sandbox_result.cleanup_status,
            "imported_changes_count": 0,
        }
        isolation_report = self._isolation_report(request.resource_limits)
        isolation_report["backend"] = sandbox_result.backend_name
        isolation_report["filesystem_isolation"] = "copy_on_write_workspace"
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
                "sandbox_backend": sandbox_result.backend_name,
                "sandbox_status": sandbox_result.status.value,
                "sandbox_trace_id": sandbox_result.trace_id,
                "sandbox_artifacts": [artifact.to_dict() for artifact in sandbox_result.artifacts],
                "sandbox_changed_files": sandbox_result.filesystem_changes.to_dict(),
                "sandbox_violations": [violation.to_dict() for violation in sandbox_result.violations],
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
            error_code=decision.error_code or "policy_denied",
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
            error_code="cwd_denied",
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
                "artifact_path": result.artifact_path,
                "changed_files": result.changed_files,
                "side_effects": result.side_effects,
                "secret_redactions": result.secret_redactions,
                "error_code": result.error_code,
                "semantic_status": result.semantic_status.value,
                "isolation_report": result.isolation_report,
                "git_before": result.git_before,
                "git_after": result.git_after,
                "policy_runtime": True,
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
            runtime="command",
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
                "phase_id": getattr(getattr(self.planner, "state", None), "current_phase", None),
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
        if self.state_runtime is not None:
            return self.state_runtime.capture_snapshot()
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
        if self.state_runtime is None:
            return []
        changes = self.state_runtime.record_command_side_effects(
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
        if backend_result.error_code in {"command_not_found", "spawn_failed", "permission_error"}:
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
            return SemanticStatus.RUNTIME_FAILED
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
            return SemanticStatus.RUNTIME_FAILED
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
            return "timeout"
        if backend_result.idle_timed_out:
            return "idle_timeout"
        if semantic_status in {
            SemanticStatus.TESTS_FAILED,
            SemanticStatus.BUILD_FAILED,
            SemanticStatus.LINT_FAILED,
            SemanticStatus.TYPECHECK_FAILED,
        }:
            return "semantic_failure"
        if semantic_status == SemanticStatus.EXIT_NONZERO:
            return "exit_nonzero"
        if output.output_truncated:
            return "output_limit_exceeded"
        return None

    @staticmethod
    def _sandbox_error_code(
        sandbox_result: SandboxResult,
        semantic_status: SemanticStatus,
    ) -> str | None:
        if sandbox_result.status == SandboxStatus.BACKEND_UNAVAILABLE:
            return "sandbox_unavailable"
        if sandbox_result.status == SandboxStatus.VIOLATION:
            return "sandbox_violation"
        if sandbox_result.status == SandboxStatus.TIMEOUT:
            return "timeout"
        if sandbox_result.metadata.get("error_code"):
            return str(sandbox_result.metadata["error_code"])
        if semantic_status in {
            SemanticStatus.TESTS_FAILED,
            SemanticStatus.BUILD_FAILED,
            SemanticStatus.LINT_FAILED,
            SemanticStatus.TYPECHECK_FAILED,
        }:
            return "semantic_failure"
        if semantic_status == SemanticStatus.EXIT_NONZERO:
            return "exit_nonzero"
        if sandbox_result.metadata.get("output_truncated"):
            return "output_limit_exceeded"
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

    def _git_state_summary(self) -> dict[str, Any]:
        git_dir = self.workspace_root / ".git"
        if not git_dir.exists():
            return {"available": False, "reason": "not_a_git_worktree"}
        head_file = git_dir / "HEAD"
        try:
            head_text = head_file.read_text(encoding="utf-8").strip()
        except OSError as exc:
            return {"available": False, "reason": str(exc)}

        branch = None
        head = head_text
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
        return PolicyRequest(
            session_id=getattr(self.planner, "session_id", "command_session"),
            task_id=getattr(self.planner, "task_id", "command_task"),
            phase_id=getattr(getattr(self.planner, "state", None), "current_phase", "command"),
            action_id=request.command_id,
            runtime=RuntimeName.COMMAND,
            operation=operation,
            capability=capability,
            subject=PolicySubject(subject_type="runtime", name="CommandRuntime"),
            resource=resource,
            reason=request.redacted_display_command(),
            proposed_by_model=True,
            metadata={
                **request.safe_metadata(),
                "env_policy": request.env_request,
                "security_mode": self.policy_runtime.config.security_mode.value,
            },
            requires_network=request.network_mode != NetworkMode.DISABLED,
            touches_workspace=request.filesystem_mode != FilesystemMode.READ_ONLY_WORKSPACE,
            long_running=request.purpose == CommandPurpose.LONG_RUNNING,
            destructive=request.purpose == CommandPurpose.DESTRUCTIVE,
            workspace_root=str(self.workspace_root),
        )

    def _policy_blocked_result(
        self,
        request: CommandRequest,
        *,
        decision: Any,
        started_at: str,
        started: float,
        git_before: dict[str, Any],
        git_after: dict[str, Any],
        policy_request: PolicyRequest,
    ) -> CommandResult:
        cmd_decision = CommandPolicyResult(
            decision=CommandDecision.REQUIRE_REVIEW
            if decision.outcome == DecisionOutcome.REQUIRE_REVIEW
            else CommandDecision.DENY,
            reasons=[decision.reason],
            risk_tags=[CommandRisk.UNKNOWN],
            required_network=request.network_mode,
            required_filesystem=request.filesystem_mode,
            redaction_rules=[],
            error_code=_policy_error_code(decision.outcome),
        )
        return self._blocked_result(
            request,
            decision=cmd_decision,
            started_at=started_at,
            started=started,
            git_before=git_before,
            git_after=git_after,
        )

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
                "runtime": policy_request.runtime.value,
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
                    "runtime": request.runtime.value,
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


def _policy_error_code(outcome: DecisionOutcome) -> str:
    mapping = {
        DecisionOutcome.DENY: "policy_denied",
        DecisionOutcome.REQUIRE_REVIEW: "review_required",
        DecisionOutcome.SANDBOX_REQUIRED: "sandbox_required",
        DecisionOutcome.ASK_USER: "policy_ask_user_required",
        DecisionOutcome.ESCALATE: "policy_escalation_required",
    }
    return mapping.get(outcome, "policy_denied")


def _policy_process_status(outcome: DecisionOutcome) -> str:
    if outcome == DecisionOutcome.REQUIRE_REVIEW:
        return "review_required"
    return _policy_error_code(outcome)
