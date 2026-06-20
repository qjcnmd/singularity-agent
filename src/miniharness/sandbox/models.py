from __future__ import annotations

from dataclasses import dataclass, field
from datetime import UTC, datetime
from enum import Enum
from pathlib import Path
from typing import Any
from uuid import uuid4

from miniharness.policy.models import PolicyConstraints


class SandboxStatus(str, Enum):
    SUCCESS = "success"
    FAILED = "failed"
    TIMEOUT = "timeout"
    POLICY_BLOCKED = "policy_blocked"
    VIOLATION = "violation"
    BACKEND_UNAVAILABLE = "backend_unavailable"
    SETUP_FAILED = "setup_failed"
    CLEANUP_FAILED = "cleanup_failed"


class SandboxFilesystemMode(str, Enum):
    NONE = "none"
    READ_ONLY_WORKSPACE = "read_only_workspace"
    COPY_ON_WRITE_WORKSPACE = "copy_on_write_workspace"
    EMPTY_TEMP_WORKSPACE = "empty_temp_workspace"
    ARTIFACT_OUTPUT_ONLY = "artifact_output_only"


class SandboxNetworkMode(str, Enum):
    DENIED = "denied"
    ALLOWED = "allowed"
    ALLOWLIST = "allowlist"
    UNSUPPORTED = "unsupported"


class SandboxProfileName(str, Enum):
    READONLY_ANALYSIS = "readonly_analysis"
    ISOLATED_VERIFICATION = "isolated_verification"
    GENERATED_CODE = "generated_code"
    PACKAGE_OPERATION = "package_operation"
    LONG_RUNNING_SERVICE = "long_running_service"


@dataclass(frozen=True)
class SandboxCapabilities:
    filesystem_isolation: bool
    copy_on_write: bool
    readonly_mount: bool
    network_isolation: bool
    env_isolation: bool
    process_tree_kill: bool
    timeout: bool
    output_limit: bool
    memory_limit: bool
    process_limit: bool
    artifact_capture: bool
    change_detection: bool

    def to_dict(self) -> dict[str, Any]:
        return self.__dict__.copy()


@dataclass
class SandboxResourceLimits:
    timeout_seconds: int | None = None
    max_output_chars: int | None = None
    max_artifact_bytes: int | None = None
    max_processes: int | None = None
    max_memory_mb: int | None = None

    def to_dict(self) -> dict[str, Any]:
        return self.__dict__.copy()


DEFAULT_SECRET_PATTERNS = [
    "*KEY*",
    "*TOKEN*",
    "*SECRET*",
    "*PASSWORD*",
    "AUTHORIZATION",
    "COOKIE",
    "NPM_TOKEN",
    "GITHUB_TOKEN",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
]


@dataclass
class SandboxEnvPolicy:
    inherit_env: bool = False
    allowlist: list[str] = field(default_factory=list)
    denylist_patterns: list[str] = field(default_factory=lambda: list(DEFAULT_SECRET_PATTERNS))
    redacted_patterns: list[str] = field(default_factory=lambda: list(DEFAULT_SECRET_PATTERNS))
    extra_env: dict[str, str] = field(default_factory=dict)
    case_insensitive: bool = True

    def to_dict(self) -> dict[str, Any]:
        return {
            "inherit_env": self.inherit_env,
            "allowlist": self.allowlist,
            "denylist_patterns": self.denylist_patterns,
            "redacted_patterns": self.redacted_patterns,
            "extra_env": {key: "[REDACTED]" for key in self.extra_env if _looks_secret(key)},
            "case_insensitive": self.case_insensitive,
        }


DEFAULT_EXCLUDE_GLOBS = [
    ".git",
    ".env",
    ".env.*",
    "*.key",
    "*.pem",
    "*.pfx",
    "*.p12",
    "*credential*",
    "*credentials*",
    "*secret*",
    "*token*",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    ".ssh",
    ".aws",
    ".azure",
    ".gnupg",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    "dist",
    "build",
    "coverage",
    ".coverage",
    "work/sandboxes",
    ".miniharness/sandboxes",
]


@dataclass
class SandboxFilesystemPolicy:
    mode: SandboxFilesystemMode = SandboxFilesystemMode.COPY_ON_WRITE_WORKSPACE
    workspace_root: Path = Path(".")
    sandbox_root: Path | None = None
    include_globs: list[str] = field(default_factory=list)
    exclude_globs: list[str] = field(default_factory=lambda: list(DEFAULT_EXCLUDE_GLOBS))
    writable_paths: list[str] = field(default_factory=list)
    readonly_paths: list[str] = field(default_factory=list)
    artifact_paths: list[str] = field(default_factory=list)
    detect_changes: bool = True

    def __post_init__(self) -> None:
        self.workspace_root = Path(self.workspace_root)
        self.sandbox_root = Path(self.sandbox_root) if self.sandbox_root is not None else None
        if not isinstance(self.mode, SandboxFilesystemMode):
            self.mode = SandboxFilesystemMode(str(self.mode))

    def to_dict(self) -> dict[str, Any]:
        return {
            "mode": self.mode.value,
            "workspace_root": str(self.workspace_root),
            "sandbox_root": str(self.sandbox_root) if self.sandbox_root else None,
            "include_globs": self.include_globs,
            "exclude_globs": self.exclude_globs,
            "writable_paths": self.writable_paths,
            "readonly_paths": self.readonly_paths,
            "artifact_paths": self.artifact_paths,
            "detect_changes": self.detect_changes,
        }


@dataclass
class SandboxNetworkPolicy:
    mode: SandboxNetworkMode = SandboxNetworkMode.DENIED
    allowed_hosts: list[str] = field(default_factory=list)
    denied_hosts: list[str] = field(default_factory=list)
    require_hard_isolation: bool = False

    def __post_init__(self) -> None:
        if not isinstance(self.mode, SandboxNetworkMode):
            self.mode = SandboxNetworkMode(str(self.mode))

    def to_dict(self) -> dict[str, Any]:
        return {
            "mode": self.mode.value,
            "allowed_hosts": self.allowed_hosts,
            "denied_hosts": self.denied_hosts,
            "require_hard_isolation": self.require_hard_isolation,
        }


@dataclass
class SandboxProfile:
    name: SandboxProfileName
    filesystem: SandboxFilesystemPolicy
    network: SandboxNetworkPolicy
    env: SandboxEnvPolicy
    resources: SandboxResourceLimits
    description: str = ""

    def __post_init__(self) -> None:
        if not isinstance(self.name, SandboxProfileName):
            self.name = SandboxProfileName(str(self.name))

    def to_dict(self) -> dict[str, Any]:
        return {
            "name": self.name.value,
            "filesystem": self.filesystem.to_dict(),
            "network": self.network.to_dict(),
            "env": self.env.to_dict(),
            "resources": self.resources.to_dict(),
            "description": self.description,
        }


@dataclass
class SandboxRequest:
    sandbox_id: str
    session_id: str
    task_id: str
    action_id: str
    command: list[str] | str
    cwd: Path
    workspace_root: Path
    profile: SandboxProfile
    policy_decision_id: str | None = None
    policy_constraints: PolicyConstraints | None = None
    reason: str | None = None
    metadata: dict[str, Any] = field(default_factory=dict)

    def __post_init__(self) -> None:
        self.cwd = Path(self.cwd)
        self.workspace_root = Path(self.workspace_root)

    def to_dict(self) -> dict[str, Any]:
        return {
            "sandbox_id": self.sandbox_id,
            "session_id": self.session_id,
            "task_id": self.task_id,
            "action_id": self.action_id,
            "command": self.command,
            "cwd": str(self.cwd),
            "workspace_root": str(self.workspace_root),
            "profile": self.profile.to_dict(),
            "policy_decision_id": self.policy_decision_id,
            "policy_constraints": (
                self.policy_constraints.to_dict() if self.policy_constraints else None
            ),
            "reason": self.reason,
            "metadata": self.metadata,
        }


@dataclass
class PreparedSandbox:
    sandbox_id: str
    backend_name: str
    sandbox_root: Path
    workspace_copy_root: Path
    execution_cwd: Path
    env: dict[str, str]
    request: SandboxRequest
    created_at: str
    trace_id: str
    baseline: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "sandbox_id": self.sandbox_id,
            "backend_name": self.backend_name,
            "sandbox_root": str(self.sandbox_root),
            "workspace_copy_root": str(self.workspace_copy_root),
            "execution_cwd": str(self.execution_cwd),
            "env": {key: "[REDACTED]" if _looks_secret(key) else value for key, value in self.env.items()},
            "created_at": self.created_at,
            "trace_id": self.trace_id,
        }


@dataclass(frozen=True)
class SandboxArtifact:
    artifact_id: str
    sandbox_id: str
    path: Path
    relative_path: str
    size_bytes: int
    kind: str = "generic"
    sha256: str = ""
    metadata: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "artifact_id": self.artifact_id,
            "artifact_ref": self.artifact_id,
            "sandbox_id": self.sandbox_id,
            "relative_path": self.relative_path,
            "relative_handle": self.relative_path,
            "size_bytes": self.size_bytes,
            "kind": self.kind,
            "sha256": self.sha256,
            "metadata": self.metadata,
        }


@dataclass(frozen=True)
class SandboxChangeSummary:
    created_files: list[str] = field(default_factory=list)
    modified_files: list[str] = field(default_factory=list)
    deleted_files: list[str] = field(default_factory=list)
    total_changed_files: int = 0
    diff_preview: str | None = None
    importable: bool = False

    def to_dict(self) -> dict[str, Any]:
        return {
            "created_files": self.created_files,
            "modified_files": self.modified_files,
            "deleted_files": self.deleted_files,
            "total_changed_files": self.total_changed_files,
            "diff_preview": self.diff_preview,
            "importable": self.importable,
        }


@dataclass(frozen=True)
class SandboxViolation:
    violation_type: str
    message: str
    severity: str
    evidence: dict[str, Any] = field(default_factory=dict)
    detected_at: str = field(default_factory=lambda: datetime.now(UTC).isoformat())

    def to_dict(self) -> dict[str, Any]:
        return {
            "violation_type": self.violation_type,
            "message": self.message,
            "severity": self.severity,
            "evidence": self.evidence,
            "detected_at": self.detected_at,
        }


@dataclass
class SandboxResult:
    sandbox_id: str
    backend_name: str
    status: SandboxStatus
    exit_code: int | None
    stdout: str
    stderr: str
    started_at: str
    ended_at: str
    duration_ms: int
    artifacts: list[SandboxArtifact] = field(default_factory=list)
    filesystem_changes: SandboxChangeSummary = field(default_factory=SandboxChangeSummary)
    violations: list[SandboxViolation] = field(default_factory=list)
    trace_id: str | None = None
    cleanup_status: str = "not_started"
    metadata: dict[str, Any] = field(default_factory=dict)

    def __post_init__(self) -> None:
        if not isinstance(self.status, SandboxStatus):
            self.status = SandboxStatus(str(self.status))

    def to_dict(self) -> dict[str, Any]:
        return {
            "sandbox_id": self.sandbox_id,
            "backend_name": self.backend_name,
            "status": self.status.value,
            "exit_code": self.exit_code,
            "stdout": self.stdout,
            "stderr": self.stderr,
            "started_at": self.started_at,
            "ended_at": self.ended_at,
            "duration_ms": self.duration_ms,
            "artifacts": [artifact.to_dict() for artifact in self.artifacts],
            "filesystem_changes": self.filesystem_changes.to_dict(),
            "violations": [violation.to_dict() for violation in self.violations],
            "trace_id": self.trace_id,
            "cleanup_status": self.cleanup_status,
            "metadata": self.metadata,
        }


def default_sandbox_profile(
    name: SandboxProfileName | str,
    *,
    workspace_root: Path,
) -> SandboxProfile:
    resolved = name if isinstance(name, SandboxProfileName) else SandboxProfileName(str(name))
    workspace = Path(workspace_root)
    if resolved == SandboxProfileName.READONLY_ANALYSIS:
        return SandboxProfile(
            name=resolved,
            filesystem=SandboxFilesystemPolicy(
                mode=SandboxFilesystemMode.COPY_ON_WRITE_WORKSPACE,
                workspace_root=workspace,
                detect_changes=False,
            ),
            network=SandboxNetworkPolicy(mode=SandboxNetworkMode.DENIED),
            env=SandboxEnvPolicy(),
            resources=SandboxResourceLimits(timeout_seconds=30, max_output_chars=20000),
            description="Read-only analysis in an isolated workspace copy.",
        )
    if resolved == SandboxProfileName.GENERATED_CODE:
        return SandboxProfile(
            name=resolved,
            filesystem=SandboxFilesystemPolicy(
                mode=SandboxFilesystemMode.EMPTY_TEMP_WORKSPACE,
                workspace_root=workspace,
                detect_changes=True,
            ),
            network=SandboxNetworkPolicy(mode=SandboxNetworkMode.DENIED),
            env=SandboxEnvPolicy(),
            resources=SandboxResourceLimits(timeout_seconds=30, max_output_chars=20000),
            description="Generated code execution in an empty temp workspace.",
        )
    if resolved == SandboxProfileName.PACKAGE_OPERATION:
        return SandboxProfile(
            name=resolved,
            filesystem=SandboxFilesystemPolicy(
                mode=SandboxFilesystemMode.COPY_ON_WRITE_WORKSPACE,
                workspace_root=workspace,
                detect_changes=True,
            ),
            network=SandboxNetworkPolicy(mode=SandboxNetworkMode.ALLOWED),
            env=SandboxEnvPolicy(),
            resources=SandboxResourceLimits(timeout_seconds=180, max_output_chars=30000),
            description="Package operation in a copy-on-write workspace.",
        )
    if resolved == SandboxProfileName.LONG_RUNNING_SERVICE:
        return SandboxProfile(
            name=resolved,
            filesystem=SandboxFilesystemPolicy(
                mode=SandboxFilesystemMode.COPY_ON_WRITE_WORKSPACE,
                workspace_root=workspace,
                detect_changes=True,
            ),
            network=SandboxNetworkPolicy(mode=SandboxNetworkMode.DENIED),
            env=SandboxEnvPolicy(),
            resources=SandboxResourceLimits(timeout_seconds=300, max_output_chars=30000),
            description="Long-running service in a temporary workspace lease.",
        )
    return SandboxProfile(
        name=SandboxProfileName.ISOLATED_VERIFICATION,
        filesystem=SandboxFilesystemPolicy(
            mode=SandboxFilesystemMode.COPY_ON_WRITE_WORKSPACE,
            workspace_root=workspace,
            detect_changes=True,
        ),
        network=SandboxNetworkPolicy(mode=SandboxNetworkMode.DENIED),
        env=SandboxEnvPolicy(),
        resources=SandboxResourceLimits(
            timeout_seconds=120,
            max_output_chars=40000,
            max_artifact_bytes=20 * 1024 * 1024,
        ),
        description="Verification in an isolated copy-on-write workspace.",
    )


def new_sandbox_id() -> str:
    return f"sandbox_{uuid4().hex[:12]}"


def _looks_secret(name: str) -> bool:
    upper = name.upper()
    return any(token in upper for token in ("KEY", "TOKEN", "SECRET", "PASSWORD", "COOKIE", "AUTHORIZATION"))
