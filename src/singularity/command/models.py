from __future__ import annotations

import hashlib
import json
import re
from dataclasses import dataclass, field, replace
from enum import Enum
from typing import Any, TypeVar
from uuid import uuid4

from singularity.observability.redaction import TraceRedactor


_COMMAND_REDACTOR = TraceRedactor()
_SECRET_ARG_FLAG_RE = re.compile(
    r"^--?(?:password|passwd|pwd|token|secret|api[-_]?key|authorization|cookie)$",
    re.IGNORECASE,
)

EnumT = TypeVar("EnumT", bound=Enum)


class CommandPurpose(str, Enum):
    READ_ONLY_COMMAND = "READ_ONLY_COMMAND"
    PROJECT_VERIFICATION = "PROJECT_VERIFICATION"
    LINT = "LINT"
    TYPECHECK = "TYPECHECK"
    FORMAT_CHECK = "FORMAT_CHECK"
    FORMATTER = "FORMATTER"
    BUILD = "BUILD"
    CODE_GENERATION = "CODE_GENERATION"
    PACKAGE_MANAGER = "PACKAGE_MANAGER"
    NETWORK = "NETWORK"
    WRITE_WORKSPACE = "WRITE_WORKSPACE"
    DESTRUCTIVE = "DESTRUCTIVE"
    LONG_RUNNING = "LONG_RUNNING"
    SECRET_RISK = "SECRET_RISK"
    VCS_READ = "VCS_READ"
    VCS_MUTATION = "VCS_MUTATION"
    SYSTEM_MUTATION = "SYSTEM_MUTATION"
    EXECUTES_PROJECT_CODE = "EXECUTES_PROJECT_CODE"
    UNKNOWN = "UNKNOWN"


class CommandRisk(str, Enum):
    READ_ONLY_COMMAND = "READ_ONLY_COMMAND"
    PROJECT_VERIFICATION = "PROJECT_VERIFICATION"
    FORMATTER = "FORMATTER"
    BUILD = "BUILD"
    CODE_GENERATION = "CODE_GENERATION"
    PACKAGE_MANAGER = "PACKAGE_MANAGER"
    NETWORK = "NETWORK"
    WRITE_WORKSPACE = "WRITE_WORKSPACE"
    DESTRUCTIVE = "DESTRUCTIVE"
    LONG_RUNNING = "LONG_RUNNING"
    SECRET_RISK = "SECRET_RISK"
    VCS_READ = "VCS_READ"
    VCS_MUTATION = "VCS_MUTATION"
    SYSTEM_MUTATION = "SYSTEM_MUTATION"
    EXECUTES_PROJECT_CODE = "EXECUTES_PROJECT_CODE"
    UNKNOWN = "UNKNOWN"


class CommandDecision(str, Enum):
    ALLOW = "allow"
    REQUIRE_REVIEW = "require_review"
    DENY = "deny"


class ExecutionStatus(str, Enum):
    COMPLETED = "completed"
    POLICY_DENIED = "policy_denied"
    REVIEW_REQUIRED = "review_required"
    SPAWN_FAILED = "spawn_failed"
    TIMED_OUT = "timed_out"
    IDLE_TIMED_OUT = "idle_timed_out"
    PROCESS_KILLED = "process_killed"
    BACKEND_ERROR = "backend_error"


class SemanticStatus(str, Enum):
    SUCCEEDED = "succeeded"
    EXIT_NONZERO = "exit_nonzero"
    TESTS_FAILED = "tests_failed"
    BUILD_FAILED = "build_failed"
    LINT_FAILED = "lint_failed"
    TYPECHECK_FAILED = "typecheck_failed"
    EXECUTION_FAILED = "execution_failed"
    POLICY_BLOCKED = "policy_blocked"


class NetworkMode(str, Enum):
    DISABLED = "DISABLED"
    ALLOW_PACKAGE_REGISTRIES = "ALLOW_PACKAGE_REGISTRIES"
    ALLOW_GIT_HOSTS = "ALLOW_GIT_HOSTS"
    ALLOW_ALL = "ALLOW_ALL"


class FilesystemMode(str, Enum):
    READ_ONLY_WORKSPACE = "READ_ONLY_WORKSPACE"
    READ_WRITE_WORKSPACE = "READ_WRITE_WORKSPACE"
    READ_WRITE_SELECTED_PATHS = "READ_WRITE_SELECTED_PATHS"
    EPHEMERAL_WORKDIR = "EPHEMERAL_WORKDIR"
    NO_HOME_ACCESS = "NO_HOME_ACCESS"
    CACHE_MOUNT = "CACHE_MOUNT"


@dataclass(frozen=True)
class ResourceLimits:
    timeout_seconds: float = 30.0
    idle_timeout_seconds: float | None = None
    max_stdout_bytes: int = 20000
    max_stderr_bytes: int = 20000
    max_combined_output_bytes: int = 40000
    max_memory_mb: int | None = None
    max_processes: int | None = None
    max_disk_write_mb: int | None = None

    def with_overrides(
        self,
        *,
        timeout_seconds: float | None,
        idle_timeout_seconds: float | None,
    ) -> "ResourceLimits":
        return replace(
            self,
            timeout_seconds=(
                float(timeout_seconds)
                if timeout_seconds is not None
                else self.timeout_seconds
            ),
            idle_timeout_seconds=(
                float(idle_timeout_seconds)
                if idle_timeout_seconds is not None
                else self.idle_timeout_seconds
            ),
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "timeout_seconds": self.timeout_seconds,
            "idle_timeout_seconds": self.idle_timeout_seconds,
            "max_stdout_bytes": self.max_stdout_bytes,
            "max_stderr_bytes": self.max_stderr_bytes,
            "max_combined_output_bytes": self.max_combined_output_bytes,
            "max_memory_mb": self.max_memory_mb,
            "max_processes": self.max_processes,
            "max_disk_write_mb": self.max_disk_write_mb,
        }


@dataclass(frozen=True)
class CommandRequest:
    argv: list[str] | None = None
    shell: str | None = None
    cwd: str = "."
    purpose: CommandPurpose = CommandPurpose.UNKNOWN
    timeout_seconds: float | None = None
    idle_timeout_seconds: float | None = None
    env_request: dict[str, str] = field(default_factory=dict)
    network_mode: NetworkMode = NetworkMode.DISABLED
    filesystem_mode: FilesystemMode = FilesystemMode.READ_ONLY_WORKSPACE
    resource_limits: ResourceLimits = field(default_factory=ResourceLimits)
    expected_outputs: list[str] = field(default_factory=list)
    risk_acceptance_reason: str | None = None
    command_id: str = field(default_factory=lambda: uuid4().hex)

    def __post_init__(self) -> None:
        object.__setattr__(self, "purpose", _enum(CommandPurpose, self.purpose))
        object.__setattr__(
            self, "network_mode", _enum(NetworkMode, self.network_mode)
        )
        object.__setattr__(
            self, "filesystem_mode", _enum(FilesystemMode, self.filesystem_mode)
        )
        object.__setattr__(
            self,
            "argv",
            [str(item) for item in self.argv] if self.argv is not None else None,
        )
        object.__setattr__(
            self,
            "resource_limits",
            self.resource_limits.with_overrides(
                timeout_seconds=self.timeout_seconds,
                idle_timeout_seconds=self.idle_timeout_seconds,
            ),
        )

    def display_command(self) -> str:
        if self.argv is not None:
            return json.dumps(self.argv, ensure_ascii=False)
        return self.shell or ""

    def redacted_display_command(self) -> str:
        return _COMMAND_REDACTOR.redact_text(self.display_command())

    def command_hash(self) -> str:
        return _hash_text(self.display_command())

    def redacted_argv(self) -> list[str] | None:
        return _redacted_argv(self.argv)

    def redacted_shell(self) -> str | None:
        return _COMMAND_REDACTOR.redact_text(self.shell) if self.shell is not None else None

    def safe_metadata(self) -> dict[str, Any]:
        return {
            "command_preview": self.redacted_display_command(),
            "command_hash": self.command_hash(),
            "argv": self.redacted_argv(),
            "shell": self.redacted_shell(),
            "cwd": self.cwd,
            "network_policy": self.network_mode.value,
            "filesystem_mode": self.filesystem_mode.value,
            "timeout": self.resource_limits.timeout_seconds,
            "long_running": self.purpose == CommandPurpose.LONG_RUNNING,
            "risk_acceptance_reason": _COMMAND_REDACTOR.redact_text(
                self.risk_acceptance_reason or ""
            )
            or None,
        }


@dataclass(frozen=True)
class CommandPolicyResult:
    decision: CommandDecision
    reasons: list[str]
    risk_tags: list[CommandRisk]
    required_backend: str = "local_process"
    required_network: NetworkMode = NetworkMode.DISABLED
    required_filesystem: FilesystemMode = FilesystemMode.READ_ONLY_WORKSPACE
    redaction_rules: list[str] = field(default_factory=list)
    error_code: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "decision": self.decision.value,
            "reasons": self.reasons,
            "risk_tags": [tag.value for tag in self.risk_tags],
            "required_backend": self.required_backend,
            "required_network": self.required_network.value,
            "required_filesystem": self.required_filesystem.value,
            "redaction_rules": self.redaction_rules,
            "error_code": self.error_code,
        }


@dataclass(frozen=True)
class CommandPlan:
    request: CommandRequest
    policy_decision: CommandPolicyResult
    cwd: str | None
    backend: str
    env_allowed: list[str] = field(default_factory=list)
    env_denied: list[str] = field(default_factory=list)
    isolation_report: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "command_id": self.request.command_id,
            "command_preview": self.request.redacted_display_command(),
            "command_hash": self.request.command_hash(),
            "argv": self.request.redacted_argv(),
            "shell": self.request.redacted_shell(),
            "cwd": self.cwd,
            "purpose": self.request.purpose.value,
            "backend": self.backend,
            "policy_decision": self.policy_decision.to_dict(),
            "env_allowed": self.env_allowed,
            "env_denied": self.env_denied,
            "network_mode": self.request.network_mode.value,
            "filesystem_mode": self.request.filesystem_mode.value,
            "resource_limits": self.request.resource_limits.to_dict(),
            "expected_outputs": self.request.expected_outputs,
            "isolation_report": self.isolation_report,
        }


@dataclass(frozen=True)
class CommandResult:
    command_id: str
    execution_status: ExecutionStatus
    semantic_status: SemanticStatus
    exit_code: int | None
    signal: int | None
    duration_ms: int
    timed_out: bool
    idle_timed_out: bool
    stdout_preview: str
    stderr_preview: str
    combined_output_preview: str
    output_truncated: bool
    output_digest: str
    artifact_path: str | None
    changed_files: list[str]
    policy_decision: CommandPolicyResult
    risk_tags: list[CommandRisk]
    error_code: str | None
    isolation_report: dict[str, Any]
    env_denied: list[str] = field(default_factory=list)
    killed_reason: str | None = None
    backend: str = "local_process"
    started_at: str | None = None
    ended_at: str | None = None
    stdout_bytes: int = 0
    stderr_bytes: int = 0
    secret_redactions: int = 0
    git_before: dict[str, Any] = field(default_factory=dict)
    git_after: dict[str, Any] = field(default_factory=dict)
    side_effects: list[dict[str, Any]] = field(default_factory=list)
    metadata: dict[str, Any] = field(default_factory=dict)

    def to_observation(self) -> dict[str, Any]:
        key_output = self.combined_output_preview or self.stderr_preview or self.stdout_preview
        if len(key_output) > 1200:
            key_output = f"{key_output[:600]}\n...[truncated]...\n{key_output[-600:]}"
        return {
            "command_result": {
                "command_id": self.command_id,
                "status": self.execution_status.value,
                "exit_code": self.exit_code,
                "semantic_status": self.semantic_status.value,
                "duration_ms": self.duration_ms,
                "summary": self._summary(),
                "key_output": key_output,
                "changed_files": self.changed_files,
                "side_effects": self.side_effects,
                "truncated": self.output_truncated,
                "artifact": self.artifact_path,
                "artifact_ref": self.artifact_path,
                "error_code": self.error_code,
                "isolation_report": self.isolation_report,
                "metadata": self.metadata,
            }
        }

    def _summary(self) -> str:
        if self.execution_status == ExecutionStatus.COMPLETED and self.exit_code == 0:
            return "Command completed successfully."
        if self.execution_status == ExecutionStatus.REVIEW_REQUIRED:
            return "Command requires review before execution."
        if self.execution_status == ExecutionStatus.POLICY_DENIED:
            return "Command was denied by policy."
        if self.timed_out:
            return "Command timed out and was killed."
        if self.idle_timed_out:
            return "Command hit idle timeout and was killed."
        if self.exit_code not in (None, 0):
            return f"Command exited with code {self.exit_code}."
        return "Command execution did not complete normally."

    def to_dict(self) -> dict[str, Any]:
        return {
            "command_id": self.command_id,
            "execution_status": self.execution_status.value,
            "semantic_status": self.semantic_status.value,
            "exit_code": self.exit_code,
            "signal": self.signal,
            "duration_ms": self.duration_ms,
            "timed_out": self.timed_out,
            "idle_timed_out": self.idle_timed_out,
            "stdout_preview": self.stdout_preview,
            "stderr_preview": self.stderr_preview,
            "combined_output_preview": self.combined_output_preview,
            "output_truncated": self.output_truncated,
            "output_digest": self.output_digest,
            "artifact_path": self.artifact_path,
            "changed_files": self.changed_files,
            "policy_decision": self.policy_decision.to_dict(),
            "risk_tags": [tag.value for tag in self.risk_tags],
            "error_code": self.error_code,
            "isolation_report": self.isolation_report,
            "env_denied": self.env_denied,
            "killed_reason": self.killed_reason,
            "backend": self.backend,
            "started_at": self.started_at,
            "ended_at": self.ended_at,
            "stdout_bytes": self.stdout_bytes,
            "stderr_bytes": self.stderr_bytes,
            "secret_redactions": self.secret_redactions,
            "git_before": self.git_before,
            "git_after": self.git_after,
            "side_effects": self.side_effects,
            "metadata": self.metadata,
        }


@dataclass(frozen=True)
class ProcessSession:
    process_id: str
    command_id: str
    pid: int | None
    status: str
    argv: list[str] | None
    shell: str | None
    cwd: str
    started_at: str
    ports: list[int] = field(default_factory=list)
    health_check: str | None = None
    logs_artifact_path: str | None = None
    owner_transaction: str | None = None
    exit_code: int | None = None
    error_code: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "process_id": self.process_id,
            "command_id": self.command_id,
            "pid": self.pid,
            "status": self.status,
            "command_preview": _redacted_command(self.argv, self.shell),
            "command_hash": _command_hash(self.argv, self.shell),
            "argv": _redacted_argv(self.argv),
            "shell": _redacted_shell(self.shell),
            "cwd": self.cwd,
            "started_at": self.started_at,
            "ports": self.ports,
            "health_check": self.health_check,
            "logs_artifact_path": self.logs_artifact_path,
            "owner_transaction": self.owner_transaction,
            "exit_code": self.exit_code,
            "error_code": self.error_code,
        }


def _redacted_argv(argv: list[str] | None) -> list[str] | None:
    if argv is None:
        return None
    redacted: list[str] = []
    redact_next = False
    for item in argv:
        text = str(item)
        if redact_next:
            redacted.append("<redacted>")
            redact_next = False
            continue
        redacted_item = _COMMAND_REDACTOR.redact_text(text)
        redacted.append(redacted_item)
        if _SECRET_ARG_FLAG_RE.match(text):
            redact_next = True
    return redacted


def _redacted_shell(shell: str | None) -> str | None:
    return _COMMAND_REDACTOR.redact_text(shell) if shell is not None else None


def _redacted_command(argv: list[str] | None, shell: str | None) -> str:
    if argv is not None:
        return _COMMAND_REDACTOR.redact_text(json.dumps(argv, ensure_ascii=False))
    return _COMMAND_REDACTOR.redact_text(shell or "")


def _command_hash(argv: list[str] | None, shell: str | None) -> str:
    if argv is not None:
        return _hash_text(json.dumps(argv, ensure_ascii=False))
    return _hash_text(shell or "")


def _hash_text(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


@dataclass(frozen=True)
class ProcessOutput:
    process_id: str
    stdout: str
    stderr: str
    combined_output: str
    truncated: bool
    artifact_path: str | None

    def to_dict(self) -> dict[str, Any]:
        return {
            "process_id": self.process_id,
            "stdout": self.stdout,
            "stderr": self.stderr,
            "combined_output": self.combined_output,
            "truncated": self.truncated,
            "artifact_path": self.artifact_path,
        }


@dataclass(frozen=True)
class ProcessStopResult:
    process_id: str
    status: str
    exit_code: int | None
    killed_reason: str | None
    changed_files: list[str] = field(default_factory=list)
    artifact_path: str | None = None
    error_code: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "process_id": self.process_id,
            "status": self.status,
            "exit_code": self.exit_code,
            "killed_reason": self.killed_reason,
            "changed_files": self.changed_files,
            "artifact_path": self.artifact_path,
            "error_code": self.error_code,
        }


def _enum(enum_type: type[EnumT], value: EnumT | str) -> EnumT:
    if isinstance(value, enum_type):
        return value
    try:
        return enum_type[str(value)]
    except KeyError:
        return enum_type(str(value))
