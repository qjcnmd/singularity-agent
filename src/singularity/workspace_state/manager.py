from __future__ import annotations

import hashlib
import os
import stat
import tempfile
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any
from uuid import uuid4

from singularity.observability.protocols import TraceRecorderProtocol
from singularity.utils.serialization import enum_value_str, utc_iso_timestamp
from singularity.workspace.pathing import (
    ResolvedWorkspacePath,
    WorkspacePathResolver,
)
from singularity.workspace.policy import FileClassifier
from singularity.workspace.snapshot import detect_line_ending, hash_bytes, looks_binary
from singularity.workspace_state.models import (
    ArtifactRecord,
    ChangeDetectionResult,
    ChangeOwnership,
    FileSnapshot,
    JournalEvent,
    RecoveryResult,
    RecoveryStatus,
    RollbackPlan,
    RollbackResult,
    WorkspaceBaseline,
    WorkspaceChange,
    WorkspaceHealthReport,
    WorkspaceHealthStatus,
)
from singularity.workspace_state.store import WorkspaceStateStore

WORKSPACE_STATE_ERROR_CODES = {
    "baseline_failed",
    "snapshot_failed",
    "path_resolution_failed",
    "file_read_failed",
    "file_hash_failed",
    "state_store_failed",
    "journal_write_failed",
    "artifact_write_failed",
    "external_change_detected",
    "snapshot_mismatch",
    "ownership_unknown",
    "rollback_not_available",
    "rollback_conflict",
    "rollback_failed",
    "session_recovery_failed",
    "corrupted_state",
    "internal_error",
}


@dataclass(frozen=True)
class WorkspaceStatePolicy:
    version: str = "local-workspace-state-v1"
    ignored_dirs: frozenset[str] = frozenset(
        {
            ".coverage",
            ".deepeval",
            ".git",
            ".hg",
            ".singularity",
            ".mypy_cache",
            ".pytest_cache",
            ".ruff_cache",
            ".svn",
            ".venv",
            "__pycache__",
            "build",
            "coverage",
            "dist",
            "node_modules",
            "outputs",
            "venv",
            "work",
        }
    )
    max_snapshot_bytes: int = 1_000_000
    classifier: FileClassifier = field(default_factory=FileClassifier)

    def should_skip(self, relative_path: Path, *, size: int | None = None) -> bool:
        lower_parts = {part.lower() for part in relative_path.parts}
        if lower_parts & self.ignored_dirs:
            return True
        return size is not None and size > self.max_snapshot_bytes


class ArtifactStore:
    def __init__(self, workspace_state_manager: WorkspaceStateManager) -> None:
        self.component = workspace_state_manager

    def save(
        self,
        *,
        kind: str,
        content: str | bytes,
        linked_command_id: str | None = None,
        linked_transaction_id: str | None = None,
        linked_verification_id: str | None = None,
        metadata: dict[str, Any] | None = None,
    ) -> ArtifactRecord:
        session_id = self.component._ensure_session()
        raw = content.encode("utf-8") if isinstance(content, str) else content
        digest = hashlib.sha256(raw).hexdigest()
        artifact_id = uuid4().hex
        safe_kind = "".join(ch if ch.isalnum() or ch in {"_", "-"} else "_" for ch in kind)
        artifact_dir = self.component.store.session_dir(session_id) / "artifacts" / safe_kind
        artifact_dir.mkdir(parents=True, exist_ok=True)
        artifact_path = artifact_dir / f"{artifact_id}.artifact"
        try:
            artifact_path.write_bytes(raw)
        except OSError as exc:
            raise WorkspaceStateError(
                "artifact_write_failed",
                f"Could not write workspace artifact: {artifact_path}",
                {"error": str(exc)},
            ) from exc
        record = ArtifactRecord(
            artifact_id=artifact_id,
            kind=kind,
            path=artifact_path.relative_to(self.component.workspace_root).as_posix(),
            digest=digest,
            size=len(raw),
            created_at=_now(),
            linked_command_id=linked_command_id,
            linked_transaction_id=linked_transaction_id,
            linked_verification_id=linked_verification_id,
            metadata=metadata or {},
        )
        self.component.store.save_artifact(session_id, record)
        self.component._append_event(
            "artifact_created",
            artifact_id=artifact_id,
            command_id=linked_command_id,
            transaction_id=linked_transaction_id,
            metadata={"artifact": record.to_dict()},
        )
        return record


class WorkspaceStateError(RuntimeError):
    def __init__(
        self,
        code: str,
        message: str,
        details: dict[str, Any] | None = None,
    ) -> None:
        super().__init__(message)
        self.code = code
        self.message = message
        self.details = details or {}


class WorkspaceStateManager:
    def __init__(
        self,
        workspace_root: Path | str,
        *,
        trace: TraceRecorderProtocol | None = None,
        policy: WorkspaceStatePolicy | None = None,
    ) -> None:
        self.workspace_root = Path(workspace_root).expanduser().resolve(strict=False)
        self.resolver = WorkspacePathResolver(self.workspace_root)
        self.policy = policy or WorkspaceStatePolicy()
        self.trace = trace
        self.store = WorkspaceStateStore(self.workspace_root)
        self.artifacts = ArtifactStore(self)
        self.session_id: str | None = None
        self.task_id: str | None = None
        self.baseline: WorkspaceBaseline | None = None

    def begin_session(
        self,
        *,
        task_id: str | None = None,
        session_id: str | None = None,
    ) -> WorkspaceBaseline:
        self.session_id = session_id or uuid4().hex
        self.task_id = task_id
        created_at = _now()
        self.store.save_session(
            session_id=self.session_id,
            task_id=task_id,
            workspace_root=str(self.workspace_root),
            status="open",
            created_at=created_at,
        )
        return self.create_baseline(task_id=task_id)

    def close_session(self, *, status: str = "closed") -> None:
        if self.session_id is None:
            return
        self._append_event("session_closed", metadata={"status": status})
        self.store.update_session_status(
            self.session_id,
            status=status,
            closed_at=_now(),
        )

    def close(self) -> None:
        self.store.close()

    def create_baseline(self, *, task_id: str | None = None) -> WorkspaceBaseline:
        session_id = self._ensure_session()
        try:
            snapshots = self.capture_snapshot()
        except Exception as exc:
            self._append_event(
                "baseline_failed",
                metadata={"error": str(exc), "code": getattr(exc, "code", None)},
            )
            raise
        baseline = WorkspaceBaseline(
            workspace_root=str(self.workspace_root),
            baseline_id=uuid4().hex,
            session_id=session_id,
            task_id=task_id if task_id is not None else self.task_id,
            created_at=_now(),
            policy_version=self.policy.version,
            snapshots=snapshots,
        )
        self.baseline = baseline
        self.store.save_baseline(baseline)
        self._append_event(
            "baseline_created",
            metadata={
                "baseline_id": baseline.baseline_id,
                "snapshot_count": len(baseline.snapshots),
                "policy_version": baseline.policy_version,
            },
        )
        for path, snapshot in baseline.snapshots.items():
            event = self._append_event(
                "file_snapshot_captured",
                path=path,
                after_snapshot=snapshot,
                ownership=ChangeOwnership.USER_OWNED,
                metadata={"baseline_id": baseline.baseline_id},
            )
            self.store.upsert_file_state(
                session_id=session_id,
                path=path,
                snapshot=snapshot,
                ownership=ChangeOwnership.USER_OWNED,
                event_id=event.event_id,
                baseline_snapshot=snapshot,
                updated_at=event.timestamp,
            )
        return baseline

    def capture_snapshot(self) -> dict[str, FileSnapshot]:
        snapshots: dict[str, FileSnapshot] = {}
        if not self.workspace_root.exists():
            return snapshots
        for path in sorted(self.workspace_root.rglob("*")):
            if not path.is_file() and not path.is_symlink():
                continue
            try:
                relative = path.relative_to(self.workspace_root)
            except ValueError:
                continue
            try:
                size = path.lstat().st_size
            except OSError:
                continue
            if self.policy.should_skip(relative, size=size):
                continue
            try:
                snapshot = self.snapshot_file(relative.as_posix())
            except WorkspaceStateError:
                continue
            snapshots[snapshot.path] = snapshot
        return snapshots

    def snapshot_file(self, user_path: str | Path) -> FileSnapshot:
        try:
            resolved = self.resolver.resolve(user_path)
        except Exception as exc:
            raise WorkspaceStateError(
                "path_resolution_failed",
                f"Could not resolve workspace path: {user_path}",
                {"error": str(exc)},
            ) from exc
        lexical = _lexical_path(self.workspace_root, user_path)
        try:
            relative = Path(os.path.relpath(str(lexical), str(self.workspace_root)))
        except ValueError:
            relative = resolved.relative_path
        if self.policy.should_skip(relative):
            raise WorkspaceStateError(
                "snapshot_failed",
                f"Path is ignored by workspace state policy: {relative.as_posix()}",
                {"path": relative.as_posix()},
            )
        return self._snapshot_from_paths(
            display_path=relative.as_posix(),
            lexical_path=lexical,
            resolved=resolved,
        )

    def detect_changes(
        self,
        *,
        before: dict[str, FileSnapshot] | None = None,
        after: dict[str, FileSnapshot] | None = None,
        ownership: ChangeOwnership = ChangeOwnership.UNKNOWN_EXTERNAL,
    ) -> ChangeDetectionResult:
        if before is None:
            before = self._last_known_snapshots()
        if after is None:
            after = self.capture_snapshot()
        changes = _diff_snapshots(before, after, ownership=ownership)
        return ChangeDetectionResult(changes=changes)

    def record_mutation(
        self,
        *,
        path: str,
        before_snapshot: Any | None,
        after_snapshot: Any | None,
        transaction_id: str | None,
        mutation_id: str | None,
        tool_call_id: str | None = None,
        before_bytes: bytes | None = None,
        metadata: dict[str, Any] | None = None,
    ) -> WorkspaceChange:
        session_id = self._ensure_session()
        before = self._coerce_snapshot(before_snapshot, path=path)
        after = self._coerce_snapshot(after_snapshot, path=path)
        event_type = _event_type_for_change(
            before=before,
            after=after,
            changed_event="file_changed_by_mutation",
        )
        rollback_artifact_path = None
        if before_bytes is not None:
            artifact = self.artifacts.save(
                kind="rollback_backup",
                content=before_bytes,
                linked_transaction_id=transaction_id,
                metadata={"path": path, "mutation_id": mutation_id},
            )
            rollback_artifact_path = artifact.path
        event = self._append_event(
            event_type,
            path=path,
            transaction_id=transaction_id,
            mutation_id=mutation_id,
            before_snapshot=before,
            after_snapshot=after,
            ownership=ChangeOwnership.AGENT_MUTATION,
            metadata={"tool_call_id": tool_call_id, **(metadata or {})},
        )
        self.store.upsert_file_state(
            session_id=session_id,
            path=path,
            snapshot=after,
            ownership=ChangeOwnership.AGENT_MUTATION,
            event_id=event.event_id,
            transaction_id=transaction_id,
            mutation_id=mutation_id,
            baseline_snapshot=self._baseline_snapshot(path),
            before_snapshot=before,
            rollback_artifact_path=rollback_artifact_path,
            updated_at=event.timestamp,
        )
        return WorkspaceChange(
            path=path,
            change_type=_change_type(before, after),
            ownership=ChangeOwnership.AGENT_MUTATION,
            before_snapshot=before,
            after_snapshot=after,
        )

    def record_rollback(
        self,
        *,
        path: str,
        before_snapshot: Any | None,
        after_snapshot: Any | None,
        transaction_id: str | None,
        mutation_id: str | None,
        metadata: dict[str, Any] | None = None,
    ) -> WorkspaceChange:
        session_id = self._ensure_session()
        before = self._coerce_snapshot(before_snapshot, path=path)
        after = self._coerce_snapshot(after_snapshot, path=path)
        event = self._append_event(
            "rollback_completed",
            path=path,
            transaction_id=transaction_id,
            mutation_id=mutation_id,
            before_snapshot=before,
            after_snapshot=after,
            ownership=ChangeOwnership.USER_OWNED,
            metadata=metadata or {},
        )
        if after is None:
            self.store.remove_file_state(session_id=session_id, path=path)
        else:
            self.store.upsert_file_state(
                session_id=session_id,
                path=path,
                snapshot=after,
                ownership=ChangeOwnership.USER_OWNED,
                event_id=event.event_id,
                transaction_id=transaction_id,
                mutation_id=mutation_id,
                baseline_snapshot=self._baseline_snapshot(path),
                before_snapshot=before,
                updated_at=event.timestamp,
            )
        return WorkspaceChange(
            path=path,
            change_type=_change_type(before, after),
            ownership=ChangeOwnership.USER_OWNED,
            before_snapshot=before,
            after_snapshot=after,
        )

    def record_command_side_effects(
        self,
        *,
        command_id: str,
        purpose: Any,
        before_snapshot: dict[str, FileSnapshot],
        after_snapshot: dict[str, FileSnapshot],
        transaction_id: str | None = None,
        metadata: dict[str, Any] | None = None,
    ) -> list[WorkspaceChange]:
        session_id = self._ensure_session()
        ownership = ownership_for_command_purpose(purpose)
        changes = _diff_snapshots(before_snapshot, after_snapshot, ownership=ownership)
        for change in changes:
            event = self._append_event(
                _event_type_for_change(
                    before=change.before_snapshot,
                    after=change.after_snapshot,
                    changed_event="file_changed_by_command",
                ),
                path=change.path,
                transaction_id=transaction_id,
                command_id=command_id,
                before_snapshot=change.before_snapshot,
                after_snapshot=change.after_snapshot,
                ownership=ownership,
                metadata={"purpose": _enum_value(purpose), **(metadata or {})},
            )
            self.store.upsert_file_state(
                session_id=session_id,
                path=change.path,
                snapshot=change.after_snapshot,
                ownership=ownership,
                event_id=event.event_id,
                transaction_id=transaction_id,
                command_id=command_id,
                baseline_snapshot=self._baseline_snapshot(change.path),
                before_snapshot=change.before_snapshot,
                updated_at=event.timestamp,
            )
        return changes

    def record_external_changes(self) -> ChangeDetectionResult:
        session_id = self._ensure_session()
        before = self._last_known_snapshots()
        after = self.capture_snapshot()
        changes = _diff_snapshots(
            before,
            after,
            ownership=ChangeOwnership.UNKNOWN_EXTERNAL,
        )
        for change in changes:
            event = self._append_event(
                "external_change_detected",
                path=change.path,
                before_snapshot=change.before_snapshot,
                after_snapshot=change.after_snapshot,
                ownership=ChangeOwnership.UNKNOWN_EXTERNAL,
            )
            self.store.upsert_file_state(
                session_id=session_id,
                path=change.path,
                snapshot=change.after_snapshot,
                ownership=ChangeOwnership.UNKNOWN_EXTERNAL,
                event_id=event.event_id,
                baseline_snapshot=self._baseline_snapshot(change.path),
                before_snapshot=change.before_snapshot,
                updated_at=event.timestamp,
            )
        return ChangeDetectionResult(
            changes=changes,
            external_changes=sorted(change.path for change in changes),
        )

    def get_workspace_health(self) -> WorkspaceHealthReport:
        if self.session_id is None:
            return WorkspaceHealthReport(
                status=WorkspaceHealthStatus.UNKNOWN,
                warnings=["No workspace state session is active."],
                recommended_next_action="begin_session",
            )
        rows = self.store.file_states(self.session_id)
        agent_changes: list[str] = []
        command_side_effects: list[str] = []
        external_changes: list[str] = []
        rollback_conflicts: list[str] = []
        command_owners = {
            ChangeOwnership.COMMAND_SIDE_EFFECT.value,
            ChangeOwnership.FORMATTER_SIDE_EFFECT.value,
            ChangeOwnership.TEST_ARTIFACT.value,
            ChangeOwnership.PACKAGE_MANAGER_SIDE_EFFECT.value,
            ChangeOwnership.GENERATED_ARTIFACT.value,
        }
        for row in rows:
            ownership = row["ownership"]
            path = row["path"]
            if ownership == ChangeOwnership.AGENT_MUTATION.value:
                agent_changes.append(path)
                if self._rollback_item_conflicts(row):
                    rollback_conflicts.append(path)
            elif ownership in command_owners:
                command_side_effects.append(path)
            elif ownership == ChangeOwnership.UNKNOWN_EXTERNAL.value:
                external_changes.append(path)

        if external_changes or rollback_conflicts:
            status = WorkspaceHealthStatus.CONFLICTED
            recommended = "re-read changed files before continuing"
        elif agent_changes or command_side_effects:
            status = WorkspaceHealthStatus.DIRTY
            recommended = "run verification or prepare rollback if needed"
        else:
            status = WorkspaceHealthStatus.CLEAN
            recommended = "continue"

        large_artifacts = [
            artifact.path
            for artifact in self.store.artifacts(self.session_id)
            if artifact.size > self.policy.max_snapshot_bytes
        ]
        return WorkspaceHealthReport(
            status=status,
            agent_changes=sorted(agent_changes),
            command_side_effects=sorted(command_side_effects),
            external_changes=sorted(external_changes),
            rollback_available=bool(agent_changes),
            rollback_conflicts=sorted(rollback_conflicts),
            large_artifacts=large_artifacts,
            warnings=[],
            recommended_next_action=recommended,
        )

    def prepare_rollback(self, *, transaction_id: str | None = None) -> RollbackPlan:
        session_id = self._ensure_session()
        items = self.store.rollback_items(
            session_id=session_id,
            transaction_id=transaction_id,
        )
        return RollbackPlan(
            plan_id=uuid4().hex,
            session_id=session_id,
            transaction_id=transaction_id,
            items=items,
            created_at=_now(),
            error_code=None if items else "rollback_not_available",
            warnings=[] if items else ["No agent-owned changes are available to roll back."],
        )

    def apply_rollback(self, plan: RollbackPlan) -> RollbackResult:
        self._append_event(
            "rollback_started",
            transaction_id=plan.transaction_id,
            metadata={"plan": plan.to_dict()},
        )
        if not plan.items:
            return RollbackResult(
                ok=False,
                status="rollback_failed",
                plan_id=plan.plan_id,
                transaction_id=plan.transaction_id,
                error_code="rollback_not_available",
                message="No agent-owned rollback items are available.",
            )
        conflicts = [item.path for item in plan.items if self._rollback_conflicts(item)]
        if conflicts:
            for path in conflicts:
                self._append_event(
                    "rollback_conflict",
                    path=path,
                    transaction_id=plan.transaction_id,
                    ownership=ChangeOwnership.AGENT_MUTATION,
                )
            return RollbackResult(
                ok=False,
                status="rollback_failed",
                plan_id=plan.plan_id,
                transaction_id=plan.transaction_id,
                conflicts=sorted(conflicts),
                error_code="rollback_conflict",
                message="Rollback would overwrite a file changed after the agent mutation.",
            )

        rolled_back: list[str] = []
        try:
            for item in plan.items:
                self._apply_rollback_item(item)
                rolled_back.append(item.path)
                after = self.snapshot_file(item.path) if item.before_snapshot else None
                event = self._append_event(
                    "rollback_completed",
                    path=item.path,
                    transaction_id=item.transaction_id,
                    mutation_id=item.mutation_id,
                    before_snapshot=item.after_snapshot,
                    after_snapshot=after,
                    ownership=ChangeOwnership.USER_OWNED,
                )
                if after is None:
                    self.store.remove_file_state(
                        session_id=plan.session_id,
                        path=item.path,
                    )
                else:
                    self.store.upsert_file_state(
                        session_id=plan.session_id,
                        path=item.path,
                        snapshot=after,
                        ownership=ChangeOwnership.USER_OWNED,
                        event_id=event.event_id,
                        baseline_snapshot=self._baseline_snapshot(item.path),
                        updated_at=event.timestamp,
                    )
        except OSError as exc:
            return RollbackResult(
                ok=False,
                status="rollback_failed",
                plan_id=plan.plan_id,
                transaction_id=plan.transaction_id,
                rolled_back_files=rolled_back,
                error_code="rollback_failed",
                message=str(exc),
            )
        return RollbackResult(
            ok=True,
            status="rolled_back",
            plan_id=plan.plan_id,
            transaction_id=plan.transaction_id,
            rolled_back_files=rolled_back,
            message="Agent-owned changes were rolled back.",
        )

    def recover_session(self, session_id: str | None = None) -> RecoveryResult:
        session = self._session_for_recovery(session_id)
        if session is None:
            return RecoveryResult(status=RecoveryStatus.CLEAN)
        self.session_id = session["session_id"]
        self.task_id = session.get("task_id")
        self.baseline = self.store.load_baseline(self.session_id)
        if self.baseline is None:
            result = RecoveryResult(
                status=RecoveryStatus.CORRUPTED,
                session_id=self.session_id,
                warnings=["Session has no baseline record."],
            )
        elif session.get("status") == "closed":
            result = RecoveryResult(status=RecoveryStatus.CLEAN, session_id=self.session_id)
        else:
            health = self.get_workspace_health()
            status = (
                RecoveryStatus.NEEDS_USER_REVIEW
                if health.external_changes or health.rollback_conflicts
                else RecoveryStatus.RECOVERABLE
            )
            result = RecoveryResult(
                status=status,
                session_id=self.session_id,
                unknown_workspace_changes=health.external_changes,
                warnings=health.warnings,
            )
        self._append_event("session_recovered", metadata={"recovery": result.to_dict()})
        return result

    def _session_for_recovery(self, session_id: str | None) -> dict[str, Any] | None:
        if session_id is not None:
            return self.store.load_session(session_id)
        open_sessions = self.store.open_sessions()
        if open_sessions:
            return open_sessions[0]
        return None

    def _snapshot_from_paths(
        self,
        *,
        display_path: str,
        lexical_path: Path,
        resolved: ResolvedWorkspacePath,
    ) -> FileSnapshot:
        try:
            stat_result = lexical_path.lstat()
        except OSError as exc:
            raise WorkspaceStateError(
                "snapshot_failed",
                f"Could not stat file: {display_path}",
                {"error": str(exc)},
            ) from exc
        is_symlink = lexical_path.is_symlink()
        symlink_target = os.readlink(lexical_path) if is_symlink else None
        try:
            raw = lexical_path.read_bytes()
        except OSError as exc:
            raise WorkspaceStateError(
                "file_read_failed",
                f"Could not read file: {display_path}",
                {"error": str(exc)},
            ) from exc
        is_binary = looks_binary(raw[:4096])
        encoding = None
        line_ending = None
        if not is_binary:
            try:
                text = raw.decode("utf-8")
                encoding = "utf-8"
                line_ending = detect_line_ending(text)
            except UnicodeDecodeError:
                is_binary = True
        resolved_for_class = ResolvedWorkspacePath(
            input_path=display_path,
            path=resolved.path,
            relative_path=Path(display_path),
            workspace_root=self.workspace_root,
        )
        file_class = self.policy.classifier.classify(
            resolved_for_class,
            size=stat_result.st_size,
            is_binary=is_binary,
        )
        return FileSnapshot(
            path=display_path,
            canonical_path=str(resolved.path),
            sha256=hash_bytes(raw),
            size=stat_result.st_size,
            mtime_ns=stat_result.st_mtime_ns,
            file_type="symlink" if is_symlink else "file",
            encoding=encoding,
            line_ending=line_ending,
            is_binary=is_binary,
            is_symlink=is_symlink,
            symlink_target=symlink_target,
            file_class=file_class,
            permissions=stat.filemode(stat_result.st_mode),
            captured_at=_now(),
        )

    def _coerce_snapshot(self, snapshot: Any | None, *, path: str) -> FileSnapshot | None:
        if snapshot is None:
            return None
        if isinstance(snapshot, FileSnapshot):
            return snapshot
        try:
            resolved = self.resolver.resolve(path)
        except Exception:
            canonical = str(self.workspace_root / path)
            file_class = "UNKNOWN"
            permissions = ""
        else:
            canonical = str(resolved.path)
            file_class = self.policy.classifier.classify(
                resolved,
                size=getattr(snapshot, "size", None),
                is_binary=getattr(snapshot, "is_binary", None),
            )
            permissions = (
                stat.filemode(resolved.path.stat().st_mode)
                if resolved.path.exists()
                else ""
            )
        return FileSnapshot(
            path=getattr(snapshot, "path", path),
            canonical_path=canonical,
            sha256=snapshot.sha256,
            size=int(snapshot.size),
            mtime_ns=int(getattr(snapshot, "mtime_ns", int(getattr(snapshot, "mtime", 0) * 1_000_000_000))),
            file_type=getattr(snapshot, "file_type", "file"),
            encoding=getattr(snapshot, "encoding", None),
            line_ending=getattr(snapshot, "line_ending", None),
            is_binary=bool(getattr(snapshot, "is_binary", False)),
            is_symlink=bool(getattr(snapshot, "is_symlink", False)),
            symlink_target=getattr(snapshot, "symlink_target", None),
            file_class=getattr(snapshot, "file_class", file_class),
            permissions=getattr(snapshot, "permissions", permissions),
            captured_at=getattr(snapshot, "captured_at", _now()),
        )

    def _last_known_snapshots(self) -> dict[str, FileSnapshot]:
        if self.session_id is None:
            return {}
        snapshots: dict[str, FileSnapshot] = {}
        for row in self.store.file_states(self.session_id):
            snapshot = FileSnapshot.from_dict(row.get("snapshot"))
            if snapshot is not None:
                snapshots[row["path"]] = snapshot
        if snapshots:
            return snapshots
        if self.baseline is not None:
            return dict(self.baseline.snapshots)
        return {}

    def _baseline_snapshot(self, path: str) -> FileSnapshot | None:
        if self.baseline is None:
            self.baseline = (
                self.store.load_baseline(self.session_id) if self.session_id else None
            )
        if self.baseline is None:
            return None
        return self.baseline.snapshots.get(path)

    def _rollback_item_conflicts(self, row: dict[str, Any]) -> bool:
        after = FileSnapshot.from_dict(row.get("snapshot"))
        item = type("Item", (), {"path": row["path"], "after_snapshot": after})
        return self._rollback_conflicts(item)

    def _rollback_conflicts(self, item: Any) -> bool:
        current_hash = self._current_hash(item.path)
        after_hash = item.after_snapshot.sha256 if item.after_snapshot else None
        return current_hash != after_hash

    def _apply_rollback_item(self, item: Any) -> None:
        resolved = self.resolver.resolve(item.path)
        if item.before_snapshot is None:
            if resolved.path.exists():
                resolved.path.unlink()
            return
        if item.before_artifact_path is None:
            raise WorkspaceStateError(
                "rollback_failed",
                f"Rollback backup is missing for {item.path}",
                {"path": item.path},
            )
        raw = (self.workspace_root / item.before_artifact_path).read_bytes()
        _atomic_write_bytes(resolved.path, raw)

    def _current_hash(self, path: str) -> str | None:
        try:
            resolved = self.resolver.resolve(path)
        except Exception:
            return None
        if not resolved.path.exists():
            return None
        try:
            return hash_bytes(resolved.path.read_bytes())
        except OSError:
            return None

    def _ensure_session(self) -> str:
        if self.session_id is None:
            self.begin_session()
        assert self.session_id is not None
        return self.session_id

    def _append_event(
        self,
        event_type: str,
        *,
        path: str | None = None,
        transaction_id: str | None = None,
        command_id: str | None = None,
        mutation_id: str | None = None,
        ownership: ChangeOwnership | None = None,
        before_snapshot: FileSnapshot | None = None,
        after_snapshot: FileSnapshot | None = None,
        artifact_id: str | None = None,
        metadata: dict[str, Any] | None = None,
    ) -> JournalEvent:
        event = JournalEvent(
            event_id=uuid4().hex,
            session_id=self._ensure_session() if event_type != "baseline_failed" else (self.session_id or ""),
            event_type=event_type,
            timestamp=_now(),
            path=path,
            transaction_id=transaction_id,
            command_id=command_id,
            mutation_id=mutation_id,
            ownership=ownership,
            before_snapshot=before_snapshot,
            after_snapshot=after_snapshot,
            artifact_id=artifact_id,
            metadata=metadata or {},
        )
        self.store.append_event(event)
        if self.trace is not None:
            self.trace.record(
                "workspace_state",
                {
                    "session_id": event.session_id,
                    "baseline_id": self.baseline.baseline_id if self.baseline else None,
                    "event_id": event.event_id,
                    "event_type": event.event_type,
                    "path": event.path,
                    "ownership": event.ownership.value if event.ownership else None,
                    "before_sha256": (
                        event.before_snapshot.sha256 if event.before_snapshot else None
                    ),
                    "after_sha256": (
                        event.after_snapshot.sha256 if event.after_snapshot else None
                    ),
                    "transaction_id": event.transaction_id,
                    "command_id": event.command_id,
                    "mutation_id": event.mutation_id,
                    "artifact_id": event.artifact_id,
                    "timestamp": event.timestamp,
                    "warning": event.metadata.get("warning"),
                    "error_code": event.metadata.get("error_code"),
                },
            )
        return event


def ownership_for_command_purpose(purpose: Any) -> ChangeOwnership:
    value = _enum_value(purpose)
    if value == "FORMATTER":
        return ChangeOwnership.FORMATTER_SIDE_EFFECT
    if value in {"PROJECT_VERIFICATION", "LINT", "TYPECHECK", "BUILD", "FORMAT_CHECK"}:
        return ChangeOwnership.TEST_ARTIFACT
    if value == "PACKAGE_MANAGER":
        return ChangeOwnership.PACKAGE_MANAGER_SIDE_EFFECT
    if value == "CODE_GENERATION":
        return ChangeOwnership.GENERATED_ARTIFACT
    return ChangeOwnership.COMMAND_SIDE_EFFECT


def _diff_snapshots(
    before: dict[str, FileSnapshot],
    after: dict[str, FileSnapshot],
    *,
    ownership: ChangeOwnership,
) -> list[WorkspaceChange]:
    changes: list[WorkspaceChange] = []
    for path in sorted(set(before) | set(after)):
        before_snapshot = before.get(path)
        after_snapshot = after.get(path)
        if before_snapshot is None and after_snapshot is None:
            continue
        if before_snapshot is None or after_snapshot is None:
            changes.append(
                WorkspaceChange(
                    path=path,
                    change_type=_change_type(before_snapshot, after_snapshot),
                    ownership=ownership,
                    before_snapshot=before_snapshot,
                    after_snapshot=after_snapshot,
                )
            )
            continue
        if before_snapshot.sha256 != after_snapshot.sha256:
            changes.append(
                WorkspaceChange(
                    path=path,
                    change_type="modified",
                    ownership=ownership,
                    before_snapshot=before_snapshot,
                    after_snapshot=after_snapshot,
                )
            )
    return changes


def _change_type(
    before: FileSnapshot | None,
    after: FileSnapshot | None,
) -> str:
    if before is None and after is not None:
        return "created"
    if before is not None and after is None:
        return "deleted"
    return "modified"


def _event_type_for_change(
    *,
    before: FileSnapshot | None,
    after: FileSnapshot | None,
    changed_event: str,
) -> str:
    if before is None and after is not None:
        return "file_created"
    if before is not None and after is None:
        return "file_deleted"
    return changed_event


_enum_value = enum_value_str


def _lexical_path(workspace_root: Path, user_path: str | Path) -> Path:
    raw = Path(user_path)
    return raw if raw.is_absolute() else workspace_root / raw


def _atomic_write_bytes(path: Path, raw: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp_path: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(delete=False, dir=path.parent) as tmp:
            tmp_path = Path(tmp.name)
            tmp.write(raw)
            tmp.flush()
            os.fsync(tmp.fileno())
        if path.exists():
            os.chmod(tmp_path, stat.S_IMODE(path.stat().st_mode))
        os.replace(tmp_path, path)
    finally:
        if tmp_path is not None and tmp_path.exists():
            tmp_path.unlink(missing_ok=True)


_now = utc_iso_timestamp
