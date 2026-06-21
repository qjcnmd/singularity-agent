from __future__ import annotations

import json
from dataclasses import dataclass, field
from enum import Enum
from typing import Any, Literal


class ChangeOwnership(str, Enum):
    USER_OWNED = "USER_OWNED"
    AGENT_MUTATION = "AGENT_MUTATION"
    COMMAND_SIDE_EFFECT = "COMMAND_SIDE_EFFECT"
    FORMATTER_SIDE_EFFECT = "FORMATTER_SIDE_EFFECT"
    TEST_ARTIFACT = "TEST_ARTIFACT"
    PACKAGE_MANAGER_SIDE_EFFECT = "PACKAGE_MANAGER_SIDE_EFFECT"
    GENERATED_ARTIFACT = "GENERATED_ARTIFACT"
    UNKNOWN_EXTERNAL = "UNKNOWN_EXTERNAL"


class WorkspaceHealthStatus(str, Enum):
    CLEAN = "clean"
    DIRTY = "dirty"
    CONFLICTED = "conflicted"
    UNKNOWN = "unknown"
    CORRUPTED = "corrupted"


class RecoveryStatus(str, Enum):
    CLEAN = "clean"
    RECOVERABLE = "recoverable"
    NEEDS_USER_REVIEW = "needs_user_review"
    CORRUPTED = "corrupted"


@dataclass(frozen=True)
class FileSnapshot:
    path: str
    canonical_path: str
    sha256: str
    size: int
    mtime_ns: int
    file_type: str
    encoding: str | None
    line_ending: Literal["lf", "crlf", "mixed", "none"] | None
    is_binary: bool
    is_symlink: bool
    symlink_target: str | None
    file_class: str
    permissions: str
    captured_at: str

    def to_dict(self) -> dict[str, Any]:
        return {
            "path": self.path,
            "canonical_path": self.canonical_path,
            "sha256": self.sha256,
            "size": self.size,
            "mtime_ns": self.mtime_ns,
            "file_type": self.file_type,
            "encoding": self.encoding,
            "line_ending": self.line_ending,
            "is_binary": self.is_binary,
            "is_symlink": self.is_symlink,
            "symlink_target": self.symlink_target,
            "file_class": self.file_class,
            "permissions": self.permissions,
            "captured_at": self.captured_at,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any] | None) -> "FileSnapshot | None":
        if payload is None:
            return None
        return cls(
            path=str(payload["path"]),
            canonical_path=str(payload["canonical_path"]),
            sha256=str(payload["sha256"]),
            size=int(payload["size"]),
            mtime_ns=int(payload["mtime_ns"]),
            file_type=str(payload["file_type"]),
            encoding=payload.get("encoding"),
            line_ending=payload.get("line_ending"),
            is_binary=bool(payload["is_binary"]),
            is_symlink=bool(payload["is_symlink"]),
            symlink_target=payload.get("symlink_target"),
            file_class=str(payload["file_class"]),
            permissions=str(payload["permissions"]),
            captured_at=str(payload["captured_at"]),
        )


@dataclass(frozen=True)
class WorkspaceBaseline:
    workspace_root: str
    baseline_id: str
    session_id: str
    task_id: str | None
    created_at: str
    policy_version: str
    snapshots: dict[str, FileSnapshot]

    def to_dict(self) -> dict[str, Any]:
        return {
            "workspace_root": self.workspace_root,
            "baseline_id": self.baseline_id,
            "session_id": self.session_id,
            "task_id": self.task_id,
            "created_at": self.created_at,
            "policy_version": self.policy_version,
            "snapshots": {
                path: snapshot.to_dict() for path, snapshot in self.snapshots.items()
            },
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "WorkspaceBaseline":
        return cls(
            workspace_root=str(payload["workspace_root"]),
            baseline_id=str(payload["baseline_id"]),
            session_id=str(payload["session_id"]),
            task_id=payload.get("task_id"),
            created_at=str(payload["created_at"]),
            policy_version=str(payload["policy_version"]),
            snapshots={
                path: snapshot
                for path, value in (payload.get("snapshots") or {}).items()
                if (snapshot := FileSnapshot.from_dict(value)) is not None
            },
        )


@dataclass(frozen=True)
class WorkspaceChange:
    path: str
    change_type: str
    ownership: ChangeOwnership
    before_snapshot: FileSnapshot | None = None
    after_snapshot: FileSnapshot | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "path": self.path,
            "change_type": self.change_type,
            "ownership": self.ownership.value,
            "before_snapshot": (
                self.before_snapshot.to_dict() if self.before_snapshot else None
            ),
            "after_snapshot": (
                self.after_snapshot.to_dict() if self.after_snapshot else None
            ),
            "before_sha256": self.before_snapshot.sha256 if self.before_snapshot else None,
            "after_sha256": self.after_snapshot.sha256 if self.after_snapshot else None,
        }


@dataclass(frozen=True)
class JournalEvent:
    event_id: str
    session_id: str
    event_type: str
    timestamp: str
    path: str | None = None
    transaction_id: str | None = None
    command_id: str | None = None
    mutation_id: str | None = None
    ownership: ChangeOwnership | None = None
    before_snapshot: FileSnapshot | None = None
    after_snapshot: FileSnapshot | None = None
    artifact_id: str | None = None
    metadata: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "event_id": self.event_id,
            "session_id": self.session_id,
            "transaction_id": self.transaction_id,
            "command_id": self.command_id,
            "mutation_id": self.mutation_id,
            "event_type": self.event_type,
            "path": self.path,
            "before_snapshot": (
                self.before_snapshot.to_dict() if self.before_snapshot else None
            ),
            "after_snapshot": (
                self.after_snapshot.to_dict() if self.after_snapshot else None
            ),
            "ownership": self.ownership.value if self.ownership else None,
            "timestamp": self.timestamp,
            "artifact_id": self.artifact_id,
            "metadata": self.metadata,
        }

    def to_json(self) -> str:
        return json.dumps(self.to_dict(), ensure_ascii=False, default=str)


@dataclass(frozen=True)
class ArtifactRecord:
    artifact_id: str
    kind: str
    path: str
    digest: str
    size: int
    created_at: str
    linked_command_id: str | None = None
    linked_transaction_id: str | None = None
    linked_verification_id: str | None = None
    metadata: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "artifact_id": self.artifact_id,
            "kind": self.kind,
            "path": self.path,
            "digest": self.digest,
            "size": self.size,
            "created_at": self.created_at,
            "linked_command_id": self.linked_command_id,
            "linked_transaction_id": self.linked_transaction_id,
            "linked_verification_id": self.linked_verification_id,
            "metadata": self.metadata,
        }


@dataclass(frozen=True)
class ChangeDetectionResult:
    changes: list[WorkspaceChange]
    external_changes: list[str] = field(default_factory=list)

    @property
    def changed_files(self) -> list[str]:
        return sorted(change.path for change in self.changes)

    def to_dict(self) -> dict[str, Any]:
        return {
            "changed_files": self.changed_files,
            "external_changes": self.external_changes,
            "changes": [change.to_dict() for change in self.changes],
        }


@dataclass(frozen=True)
class RollbackItem:
    path: str
    transaction_id: str | None
    mutation_id: str | None
    before_snapshot: FileSnapshot | None
    after_snapshot: FileSnapshot | None
    before_artifact_path: str | None

    def to_dict(self) -> dict[str, Any]:
        return {
            "path": self.path,
            "transaction_id": self.transaction_id,
            "mutation_id": self.mutation_id,
            "before_snapshot": (
                self.before_snapshot.to_dict() if self.before_snapshot else None
            ),
            "after_snapshot": self.after_snapshot.to_dict() if self.after_snapshot else None,
            "before_artifact_path": self.before_artifact_path,
        }


@dataclass(frozen=True)
class RollbackPlan:
    plan_id: str
    session_id: str
    transaction_id: str | None
    items: list[RollbackItem]
    created_at: str
    error_code: str | None = None
    warnings: list[str] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {
            "plan_id": self.plan_id,
            "session_id": self.session_id,
            "transaction_id": self.transaction_id,
            "items": [item.to_dict() for item in self.items],
            "created_at": self.created_at,
            "error_code": self.error_code,
            "warnings": self.warnings,
        }


@dataclass(frozen=True)
class RollbackResult:
    ok: bool
    status: str
    plan_id: str
    transaction_id: str | None = None
    rolled_back_files: list[str] = field(default_factory=list)
    conflicts: list[str] = field(default_factory=list)
    error_code: str | None = None
    message: str = ""

    def to_dict(self) -> dict[str, Any]:
        return {
            "ok": self.ok,
            "status": self.status,
            "plan_id": self.plan_id,
            "transaction_id": self.transaction_id,
            "rolled_back_files": self.rolled_back_files,
            "conflicts": self.conflicts,
            "error_code": self.error_code,
            "message": self.message,
        }


@dataclass(frozen=True)
class RecoveryResult:
    status: RecoveryStatus
    session_id: str | None = None
    incomplete_transactions: list[str] = field(default_factory=list)
    pending_rollbacks: list[str] = field(default_factory=list)
    unknown_workspace_changes: list[str] = field(default_factory=list)
    warnings: list[str] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {
            "status": self.status.value,
            "session_id": self.session_id,
            "incomplete_transactions": self.incomplete_transactions,
            "pending_rollbacks": self.pending_rollbacks,
            "unknown_workspace_changes": self.unknown_workspace_changes,
            "warnings": self.warnings,
        }


@dataclass(frozen=True)
class WorkspaceHealthReport:
    status: WorkspaceHealthStatus
    agent_changes: list[str] = field(default_factory=list)
    command_side_effects: list[str] = field(default_factory=list)
    external_changes: list[str] = field(default_factory=list)
    rollback_available: bool = False
    rollback_conflicts: list[str] = field(default_factory=list)
    large_artifacts: list[str] = field(default_factory=list)
    warnings: list[str] = field(default_factory=list)
    recommended_next_action: str = "continue"

    def to_dict(self) -> dict[str, Any]:
        return {
            "status": self.status.value,
            "agent_changes": self.agent_changes,
            "command_side_effects": self.command_side_effects,
            "external_changes": self.external_changes,
            "rollback_available": self.rollback_available,
            "rollback_conflicts": self.rollback_conflicts,
            "large_artifacts": self.large_artifacts,
            "warnings": self.warnings,
            "recommended_next_action": self.recommended_next_action,
        }

    def to_observation(self) -> dict[str, Any]:
        return {"workspace_state": self.to_dict()}
