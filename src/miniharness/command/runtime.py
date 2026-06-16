from __future__ import annotations

import hashlib
import os
import time
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

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
from miniharness.trace import TraceWriter


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
    before_snapshot: WorkspaceSnapshot


class CommandRuntime:
    def __init__(
        self,
        workspace_root: Path | str,
        *,
        policy: CommandPolicy | None = None,
        backend: ExecutionBackend | None = None,
        trace: TraceWriter | None = None,
        env_policy: EnvPolicy | None = None,
    ) -> None:
        self.workspace_root = Path(workspace_root).expanduser().resolve(strict=False)
        self.policy = policy or CommandPolicy()
        self.backend = backend or LocalProcessBackend()
        self.trace = trace
        self.env_policy = env_policy or EnvPolicy()
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
        started_at = _now()
        started = time.perf_counter()
        before_snapshot = WorkspaceSnapshot.capture(self.workspace_root)
        git_before = self._git_state_summary()
        decision = self.policy.evaluate(request, workspace_root=self.workspace_root)
        if decision.decision != CommandDecision.ALLOW:
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

        env_result = self.env_policy.build(request.env_request)
        collector = self._collector(
            request.command_id,
            request.resource_limits,
            env_result.redactor,
        )
        backend_result = self.backend.execute(
            request=request,
            cwd=cwd,
            env=env_result.env,
            collector=collector,
        )
        snapshot = collector.snapshot()
        after_snapshot = WorkspaceSnapshot.capture(self.workspace_root)
        git_after = self._git_state_summary()
        result = self._completed_result(
            request,
            decision=decision,
            backend_result=backend_result,
            output=snapshot,
            changed_files=before_snapshot.changed_files(after_snapshot),
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
        return result

    def start_process(
        self,
        request: CommandRequest,
        *,
        tool_call_id: str | None = None,
        transaction_id: str | None = None,
    ) -> ProcessSession:
        started_at = _now()
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
            before_snapshot=WorkspaceSnapshot.capture(self.workspace_root),
        )
        return session

    def read_process_output(self, process_id: str) -> ProcessOutput:
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
        after_snapshot = WorkspaceSnapshot.capture(self.workspace_root)
        changed_files = record.before_snapshot.changed_files(after_snapshot)
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

    def _completed_result(
        self,
        request: CommandRequest,
        *,
        decision: CommandPolicyResult,
        backend_result: BackendRunResult,
        output: OutputSnapshot,
        changed_files: list[str],
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
        self.trace.record(
            "command",
            {
                "command_id": request.command_id,
                "tool_call_id": tool_call_id,
                "transaction_id": transaction_id,
                "argv": request.argv,
                "shell": request.shell,
                "cwd": request.cwd,
                "backend": result.backend,
                "sandbox_id": None,
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
                "secret_redactions": result.secret_redactions,
                "error_code": result.error_code,
                "semantic_status": result.semantic_status.value,
                "isolation_report": result.isolation_report,
                "git_before": result.git_before,
                "git_after": result.git_after,
            },
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
        if request.purpose == CommandPurpose.FORMATTER:
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


def _now() -> str:
    return datetime.now(UTC).isoformat()


def _relative_or_absolute(path: Path, root: Path) -> str:
    try:
        return path.relative_to(root).as_posix() or "."
    except ValueError:
        return str(path)
