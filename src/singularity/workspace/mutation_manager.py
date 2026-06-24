from __future__ import annotations

import json
import difflib
import os
import shutil
import tempfile
import time
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from typing import TYPE_CHECKING, Any
from uuid import uuid4

from singularity.observability.protocols import TraceEmitterProtocol
from singularity.observability.models import TraceEventType
from singularity.policy import (
    Capability,
    DecisionOutcome,
    OperationKind,
    PolicyConfig,
    PolicyRequest,
    PolicyEngine,
    PolicySubject,
    ResourceRef,
    PolicyComponent,
)
from singularity.policy.audit import redact
from singularity.workspace.diff import (
    DiffEngine,
    FileDiff,
    UnifiedDiffError,
    apply_unified_diff_to_text,
    parse_unified_diff,
)
from singularity.workspace.errors import MutationError
from singularity.workspace.git import GitState, collect_git_state
from singularity.workspace.operations import (
    ApplyUnifiedDiff,
    CreateFile,
    DeleteFile,
    EditOperation,
    FormatFile,
    InsertAfter,
    InsertBefore,
    MoveFile,
    ReplaceRange,
    ReplaceText,
    UpdateJson,
    UpdateToml,
    UpdateYaml,
    operation_expected_sha,
    operation_paths,
    operation_type,
)
from singularity.workspace.pathing import ResolvedWorkspacePath, WorkspacePathResolver
from singularity.workspace.policy import ALLOW, DENY, REQUIRE_REVIEW, PolicyDecision, WorkspacePolicy
from singularity.workspace.snapshot import FileSnapshot, WorkspaceIndex, hash_bytes

if TYPE_CHECKING:
    from singularity.workspace_state import WorkspaceStateManager


@dataclass(frozen=True)
class JournalEntry:
    transaction_id: str
    changeset_id: str
    operation_id: str
    path: str
    before_sha256: str | None
    after_sha256: str | None
    before_artifact_path: str | None
    action: str


@dataclass
class ChangeSet:
    id: str
    base_snapshots: dict[str, FileSnapshot | None]
    operations: list[EditOperation]
    affected_files: list[str]
    final_texts: dict[str, str | None]
    intent: str
    risk_level: str
    created_at: str
    created_by: str
    policy_decisions: list[PolicyDecision]
    diffs: list[FileDiff]

    def preview(self) -> list[dict[str, object]]:
        return [diff.summary() for diff in self.diffs]

    def validate(self) -> None:
        if not self.operations:
            raise MutationError("invalid_operation", "ChangeSet has no operations.")

    def apply(self, manager: "WorkspaceMutationManager") -> "MutationResult":
        return manager.apply_changeset(self)

    def reject(self, reason: str = "Rejected before apply.") -> "MutationResult":
        return MutationResult(
            ok=False,
            status="rejected",
            error_code="policy_denied",
            message=reason,
            changeset_id=self.id,
            policy_decisions=self.policy_decisions,
            diffs=self.diffs,
        )

    def rollback(self, manager: "WorkspaceMutationManager", transaction_id: str) -> "MutationResult":
        return RollbackManager(manager).rollback(transaction_id)

    def explain(self) -> dict[str, object]:
        return {
            "changeset_id": self.id,
            "intent": self.intent,
            "risk_level": self.risk_level,
            "affected_files": self.affected_files,
            "operations": [operation_type(operation) for operation in self.operations],
            "policy": [
                {
                    "decision": decision.decision,
                    "file_class": decision.file_class,
                    "reasons": decision.reasons,
                    "risk_tags": decision.risk_tags,
                }
                for decision in self.policy_decisions
            ],
            "diffs": self.preview(),
        }


@dataclass
class MutationResult:
    ok: bool
    status: str
    error_code: str | None = None
    message: str = ""
    changeset_id: str | None = None
    transaction_id: str | None = None
    operation_id: str | None = None
    affected_files: list[str] = field(default_factory=list)
    diffs: list[FileDiff] = field(default_factory=list)
    policy_decisions: list[PolicyDecision] = field(default_factory=list)
    observation: dict[str, Any] = field(default_factory=dict)
    verification_status: str = "not_run"
    git_before: GitState | None = None
    git_after: GitState | None = None


class AtomicWriter:
    def write_text(
        self,
        path: Path,
        text: str,
        *,
        snapshot: FileSnapshot | None,
    ) -> FileSnapshot:
        line_ending = snapshot.line_ending if snapshot is not None else "lf"
        encoding = snapshot.encoding if snapshot is not None else "utf-8"
        normalized = normalize_line_endings(text, line_ending)
        raw = normalized.encode(encoding or "utf-8")
        self.write_bytes(path, raw, snapshot=snapshot)
        return FileSnapshot.from_path(path, relative_path=snapshot.path if snapshot else path.name)

    def write_bytes(
        self,
        path: Path,
        raw: bytes,
        *,
        snapshot: FileSnapshot | None,
    ) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp_path: Path | None = None
        try:
            with tempfile.NamedTemporaryFile(delete=False, dir=path.parent) as tmp:
                tmp_path = Path(tmp.name)
                tmp.write(raw)
                tmp.flush()
                os.fsync(tmp.fileno())
            if path.exists():
                shutil.copymode(path, tmp_path, follow_symlinks=False)
            os.replace(tmp_path, path)
        except Exception as exc:
            if tmp_path is not None and tmp_path.exists():
                tmp_path.unlink(missing_ok=True)
            if isinstance(exc, MutationError):
                raise
            raise MutationError(
                "atomic_write_failed",
                f"Atomic write failed for {path}",
                {"error": str(exc)},
            ) from exc


class MutationJournal:
    def __init__(self, manager: "WorkspaceMutationManager", transaction_id: str) -> None:
        self.component = manager
        self.transaction_id = transaction_id
        self.dir = manager.workspace_root / ".singularity" / "journals" / transaction_id
        self.dir.mkdir(parents=True, exist_ok=True)
        self.path = self.dir / "journal.jsonl"
        self.entries: list[JournalEntry] = []

    def before_artifact(self, relative_path: str, raw: bytes) -> str:
        artifact_name = f"{hash_bytes(raw)}.before"
        artifact_path = self.dir / artifact_name
        artifact_path.write_bytes(raw)
        return artifact_path.relative_to(self.component.workspace_root).as_posix()

    def append(self, entry: JournalEntry) -> None:
        self.entries.append(entry)
        with self.path.open("a", encoding="utf-8") as file:
            file.write(json.dumps(entry.__dict__, ensure_ascii=False) + "\n")


class WorkspaceMutationManager:
    def __init__(
        self,
        workspace_root: Path | str,
        *,
        policy: WorkspacePolicy | None = None,
        trace: TraceEmitterProtocol | None = None,
        diff_context_lines: int = 3,
        max_inline_diff_lines: int = 200,
        verification_hook: Any | None = None,
        workspace_state_manager: "WorkspaceStateManager | None" = None,
        planner: Any | None = None,
        policy_engine: PolicyEngine | None = None,
        project_index: Any | None = None,
    ) -> None:
        self.workspace_root = Path(workspace_root).resolve(strict=False)
        self.resolver = WorkspacePathResolver(self.workspace_root)
        self.index = WorkspaceIndex(self.workspace_root)
        self.policy = policy or WorkspacePolicy()
        self.trace = trace
        self.diff_engine = DiffEngine(
            self.workspace_root,
            context_lines=diff_context_lines,
            max_inline_lines=max_inline_diff_lines,
        )
        self.atomic_writer = AtomicWriter()
        self.verification_hook = verification_hook
        self.workspace_state_manager = workspace_state_manager
        self.planner = planner
        self.policy_engine = policy_engine or PolicyEngine(
            PolicyConfig.default_for_workspace(self.workspace_root)
        )
        self.project_index = project_index
        self._journals: dict[str, MutationJournal] = {}
        self._changesets: dict[str, ChangeSet] = {}
        self._changeset_transactions: dict[str, str] = {}
        self._changeset_results: dict[str, MutationResult] = {}
        self._changeset_order: list[str] = []

    def preview_operations(
        self,
        operations: list[EditOperation],
        *,
        intent: str,
        created_by: str,
        tool_call_id: str | None = None,
    ) -> MutationResult:
        try:
            changeset = self.create_changeset(
                operations,
                intent=intent,
                created_by=created_by,
            )
        except MutationError as exc:
            return self._failure_result(exc, status="rejected")
        policy_result = self._policy_result(changeset)
        if policy_result is not None:
            return policy_result
        return MutationResult(
            ok=True,
            status="preview",
            message="ChangeSet preview is valid.",
            changeset_id=changeset.id,
            affected_files=changeset.affected_files,
            diffs=changeset.diffs,
            policy_decisions=changeset.policy_decisions,
            observation=self._observation("preview", changeset),
        )

    def apply_operations(
        self,
        operations: list[EditOperation],
        *,
        intent: str,
        created_by: str,
        tool_call_id: str | None = None,
    ) -> MutationResult:
        started = time.perf_counter()
        try:
            changeset = self.create_changeset(
                operations,
                intent=intent,
                created_by=created_by,
            )
        except MutationError as exc:
            result = self._failure_result(exc, status="rejected")
            self._record_failure_trace(result, tool_call_id=tool_call_id, started=started)
            return result

        self._emit_observability(
            TraceEventType.MUTATION_PROPOSED,
            summary=f"Mutation proposed for {len(changeset.affected_files)} file(s).",
            payload={
                "changeset_id": changeset.id,
                "intent": changeset.intent,
                "affected_files": changeset.affected_files,
                "risk_level": changeset.risk_level,
                "diff_summary": [diff.summary() for diff in changeset.diffs],
            },
            transaction_id=None,
            action_id=tool_call_id,
        )
        policy_result = self._policy_result(changeset)
        if policy_result is not None:
            self._record_failure_trace(policy_result, tool_call_id=tool_call_id, started=started)
            return policy_result
        return self.apply_changeset(changeset, tool_call_id=tool_call_id, started=started)

    def create_changeset(
        self,
        operations: list[EditOperation],
        *,
        intent: str,
        created_by: str,
    ) -> ChangeSet:
        if not operations:
            raise MutationError("invalid_operation", "At least one operation is required.")

        base_snapshots: dict[str, FileSnapshot | None] = {}
        base_texts: dict[str, str] = {}
        final_texts: dict[str, str | None] = {}
        resolved_paths: dict[str, ResolvedWorkspacePath] = {}
        operation_by_path: dict[str, EditOperation] = {}

        for operation in operations:
            for path in operation_paths(operation):
                resolved = self.resolver.resolve(path)
                resolved_paths[resolved.relative_posix] = resolved
                operation_by_path.setdefault(resolved.relative_posix, operation)
                if resolved.relative_posix not in base_snapshots:
                    if (
                        isinstance(operation, MoveFile)
                        and path == operation.new_path
                        and not resolved.path.exists()
                    ):
                        snapshot = None
                    else:
                        snapshot = self._snapshot_for_operation(operation, resolved)
                    base_snapshots[resolved.relative_posix] = snapshot
                    if snapshot is not None:
                        base_texts[resolved.relative_posix] = self._read_text(resolved, snapshot)
                        final_texts[resolved.relative_posix] = base_texts[resolved.relative_posix]
                    else:
                        final_texts[resolved.relative_posix] = None

        for operation in operations:
            self._assert_expected_snapshot(operation, base_snapshots, resolved_paths)
            self._apply_operation(operation, base_texts, final_texts, resolved_paths)

        affected_files = sorted(
            path
            for path, final_text in final_texts.items()
            if base_texts.get(path) != final_text
        )
        diffs = [
            self.diff_engine.text_diff(
                path=path,
                before_text=base_texts.get(path, ""),
                after_text=final_texts[path] or "",
            )
            for path in affected_files
        ]
        decisions: list[PolicyDecision] = []
        diff_by_path = {diff.path: diff for diff in diffs}
        for path in sorted(resolved_paths):
            operation = operation_by_path[path]
            snapshot = base_snapshots.get(path)
            diff = diff_by_path.get(path)
            decisions.append(
                self.policy.check(
                    operation_type=operation_type(operation),
                    resolved=resolved_paths[path],
                    size=snapshot.size if snapshot else None,
                    is_binary=snapshot.is_binary if snapshot else None,
                    added_lines=diff.added_lines if diff else 0,
                    removed_lines=diff.removed_lines if diff else 0,
                    task_intent=intent,
                )
            )
        risk_level = self._risk_level(decisions, diffs)
        return ChangeSet(
            id=uuid4().hex,
            base_snapshots=base_snapshots,
            operations=operations,
            affected_files=affected_files,
            final_texts={path: final_texts[path] for path in affected_files},
            intent=intent,
            risk_level=risk_level,
            created_at=_now(),
            created_by=created_by,
            policy_decisions=decisions,
            diffs=diffs,
        )

    def apply_file_updates(
        self,
        updates: dict[str, str],
        *,
        intent: str,
        created_by: str,
        tool_call_id: str | None = None,
    ) -> MutationResult:
        operations: list[EditOperation] = []
        for path, content in updates.items():
            resolved = self.resolver.resolve(path)
            if resolved.path.exists():
                try:
                    before = resolved.path.read_text(encoding="utf-8")
                except UnicodeDecodeError as exc:
                    raise MutationError(
                        "encoding_error",
                        f"Could not decode file: {resolved.relative_posix}",
                        {"path": resolved.relative_posix},
                    ) from exc
                operations.append(
                    ApplyUnifiedDiff(
                        path=resolved.relative_posix,
                        diff=_make_unified_diff(resolved.relative_posix, before, content),
                    )
                )
            else:
                operations.append(CreateFile(path=resolved.relative_posix, content=content))
        return self.apply_operations(
            operations,
            intent=intent,
            created_by=created_by,
            tool_call_id=tool_call_id,
        )

    def operations_from_unified_diff(
        self,
        patch: str,
        *,
        expected_files: list[str] | None = None,
        allow_new_files: bool = True,
    ) -> list[ApplyUnifiedDiff]:
        try:
            patches = parse_unified_diff(patch)
        except UnifiedDiffError as exc:
            raise MutationError(exc.code, str(exc), {"path": exc.path}) from exc
        expected = {self.resolver.resolve(path).relative_posix for path in expected_files or []}
        paths = {self.resolver.resolve(item.path).relative_posix for item in patches}
        if expected and paths != expected:
            raise MutationError(
                "unexpected_patch_files",
                "Patch touched files outside expected_files.",
                {"expected_files": sorted(expected), "actual_files": sorted(paths)},
            )
        operations: list[ApplyUnifiedDiff] = []
        for item in patches:
            resolved = self.resolver.resolve(item.path)
            if item.is_binary or item.is_rename or item.is_delete:
                raise MutationError(
                    "unsupported_operation",
                    "Only text create/modify patches are supported.",
                    {"path": item.path},
                )
            if item.is_new_file and not allow_new_files:
                raise MutationError(
                    "new_file_not_allowed",
                    "Patch creates a file but allow_new_files is false.",
                    {"path": item.path},
                )
            if item.is_new_file and resolved.path.exists():
                raise MutationError(
                    "file_changed",
                    "Patch creates a file that already exists.",
                    {"path": resolved.relative_posix},
                )
            operations.append(
                ApplyUnifiedDiff(
                    path=resolved.relative_posix,
                    diff=item.text,
                )
            )
        return operations

    def rollback_changeset(self, changeset_id: str, reason: str | None = None) -> MutationResult:
        transaction_id = self._changeset_transactions.get(changeset_id)
        if transaction_id is None:
            return MutationResult(
                ok=False,
                status="rollback_failed",
                error_code="rollback_failed",
                message=f"Unknown changeset: {changeset_id}",
                changeset_id=changeset_id,
            )
        changeset = self._changesets.get(changeset_id)
        policy_result = self._rollback_policy_result(
            changeset_id=changeset_id,
            transaction_id=transaction_id,
            reason=reason,
            changed_files=changeset.affected_files if changeset else [],
        )
        if policy_result is not None:
            self._remember_changeset_result(policy_result)
            return policy_result
        result = RollbackManager(self).rollback(transaction_id)
        result.changeset_id = changeset_id
        result.affected_files = changeset.affected_files if changeset else []
        result.observation = {
            "mutation_status": result.status,
            "changeset_id": changeset_id,
            "transaction_id": transaction_id,
            "changed_files": result.affected_files,
            "diff_summary": [],
            "diff_digest": _digest_diffs([]),
            "artifact_refs": [f"changeset:{changeset_id}"],
            "warnings": [] if result.ok else [result.message],
            "error_code": result.error_code,
        }
        if result.ok and self.planner is not None:
            self.planner.update_from_mutation(
                result.observation,
                tool_call_id=None,
            )
        self._remember_changeset_result(result)
        return result

    def inspect_diff(
        self,
        *,
        scope: str = "current_run",
        changeset_id: str | None = None,
        paths: list[str] | None = None,
    ) -> dict[str, Any]:
        if scope == "changeset" and not changeset_id:
            raise MutationError(
                "invalid_operation",
                "changeset_id is required when scope is changeset.",
            )
        path_filter = {self.resolver.resolve(path).relative_posix for path in paths or []}
        ids = [changeset_id] if scope == "changeset" and changeset_id else list(self._changeset_order)
        diffs: list[FileDiff] = []
        classes: dict[str, set[str]] = {
            "added_files": set(),
            "modified_files": set(),
            "deleted_files": set(),
        }
        for item_id in ids:
            result = self._changeset_results.get(str(item_id))
            if result is None or not result.ok:
                continue
            change_classes = self._changeset_file_classes(str(item_id))
            for diff in result.diffs:
                if not path_filter or diff.path in path_filter:
                    diffs.append(diff)
                    for key, values in change_classes.items():
                        if diff.path in values:
                            classes[key].add(diff.path)
        changed = sorted({diff.path for diff in diffs})
        return {
            "scope": scope,
            "changeset_id": changeset_id,
            "changed_files": changed,
            "added_files": sorted(classes["added_files"]),
            "modified_files": sorted(classes["modified_files"]),
            "deleted_files": sorted(classes["deleted_files"]),
            "diff_excerpt": _diff_excerpt(diffs),
            "diff_digest": _digest_diffs(diffs),
            "artifact_refs": self._artifact_refs(ids=[str(item_id) for item_id in ids if item_id], diffs=diffs),
            "warnings": [],
        }

    def _changeset_file_classes(self, changeset_id: str) -> dict[str, set[str]]:
        changeset = self._changesets.get(changeset_id)
        if changeset is None:
            return {"added_files": set(), "modified_files": set(), "deleted_files": set()}
        added: set[str] = set()
        modified: set[str] = set()
        deleted: set[str] = set()
        for path in changeset.affected_files:
            if changeset.final_texts.get(path) is None:
                deleted.add(path)
            elif changeset.base_snapshots.get(path) is None:
                added.add(path)
            else:
                modified.add(path)
        return {
            "added_files": added,
            "modified_files": modified,
            "deleted_files": deleted,
        }

    def _remember_changeset_result(self, result: MutationResult) -> None:
        if result.changeset_id is None:
            return
        self._changeset_results[result.changeset_id] = result
        if result.status == "rolled_back":
            if result.changeset_id in self._changeset_order:
                self._changeset_order.remove(result.changeset_id)
            return
        if result.ok and result.changeset_id not in self._changeset_order:
            self._changeset_order.append(result.changeset_id)

    def _artifact_refs(self, *, ids: list[str], diffs: list[FileDiff]) -> list[str]:
        refs = [f"workspace:{path}" for path in sorted({diff.path for diff in diffs})]
        refs.extend(f"changeset:{item_id}" for item_id in ids)
        refs.extend(f"diff:{diff.digest}" for diff in diffs if diff.digest)
        refs.extend(f"workspace:{diff.artifact_path}" for diff in diffs if diff.artifact_path)
        return refs

    def apply_changeset(
        self,
        changeset: ChangeSet,
        *,
        tool_call_id: str | None = None,
        started: float | None = None,
    ) -> MutationResult:
        started = started or time.perf_counter()
        transaction_id = uuid4().hex
        journal = MutationJournal(self, transaction_id)
        self._journals[transaction_id] = journal
        self._changesets[changeset.id] = changeset
        git_before = collect_git_state(self.workspace_root)
        applied: list[JournalEntry] = []
        self._emit_observability(
            TraceEventType.MUTATION_TRANSACTION_STARTED,
            summary=f"Mutation transaction started for {len(changeset.affected_files)} file(s).",
            payload={
                "changeset_id": changeset.id,
                "affected_files": changeset.affected_files,
                "risk_level": changeset.risk_level,
            },
            transaction_id=transaction_id,
            action_id=tool_call_id,
        )

        try:
            changeset.validate()
            self._preflight_current_state(changeset)
            policy_decision = self._enforce_policy(changeset, transaction_id)
            if policy_decision is not None:
                result = policy_decision
                self._remember_changeset_result(result)
                self._record_failure_trace(result, tool_call_id=tool_call_id, started=started)
                return result
            for path in changeset.affected_files:
                resolved = self.resolver.resolve(path)
                before_snapshot = changeset.base_snapshots.get(path)
                before_raw = resolved.path.read_bytes() if resolved.path.exists() else None
                before_artifact = (
                    journal.before_artifact(path, before_raw)
                    if before_raw is not None
                    else None
                )
                final_text = changeset.final_texts[path]
                if final_text is None:
                    if resolved.path.exists():
                        resolved.path.unlink()
                    action = "delete"
                else:
                    self.atomic_writer.write_text(
                        resolved.path,
                        final_text,
                        snapshot=before_snapshot,
                    )
                    action = "write"
                after_snapshot = (
                    FileSnapshot.from_path(resolved.path, relative_path=path)
                    if resolved.path.exists()
                    else None
                )
                operation = self._operation_for_path(changeset, path)
                entry = JournalEntry(
                    transaction_id=transaction_id,
                    changeset_id=changeset.id,
                    operation_id=getattr(operation, "id", ""),
                    path=path,
                    before_sha256=before_snapshot.sha256 if before_snapshot else None,
                    after_sha256=after_snapshot.sha256 if after_snapshot else None,
                    before_artifact_path=before_artifact,
                    action=action,
                )
                journal.append(entry)
                applied.append(entry)
                if self.workspace_state_manager is not None:
                    self.workspace_state_manager.record_mutation(
                        path=path,
                        before_snapshot=before_snapshot,
                        after_snapshot=after_snapshot,
                        transaction_id=transaction_id,
                        mutation_id=entry.operation_id,
                        tool_call_id=tool_call_id,
                        before_bytes=before_raw,
                        metadata={"changeset_id": changeset.id, "action": action},
                    )
                self._record_mutation_trace(
                    changeset=changeset,
                    transaction_id=transaction_id,
                    entry=entry,
                    tool_call_id=tool_call_id,
                    status="applied",
                    error_code=None,
                    duration_ms=int((time.perf_counter() - started) * 1000),
                )
        except Exception as exc:
            rollback_error = self._rollback_entries(applied)
            error_code = "transaction_failed"
            message = str(exc)
            details = {
                "cause_code": getattr(exc, "code", None),
                "cause": message,
                "rollback_error": rollback_error,
            }
            result = MutationResult(
                ok=False,
                status="rejected",
                error_code=error_code,
                message=message,
                changeset_id=changeset.id,
                transaction_id=transaction_id,
                affected_files=changeset.affected_files,
                diffs=changeset.diffs,
                policy_decisions=changeset.policy_decisions,
                observation=self._observation(
                    "failed",
                    changeset,
                    error_code=error_code,
                    error_details=details,
                ),
                git_before=git_before,
                git_after=collect_git_state(self.workspace_root),
            )
            self._remember_changeset_result(result)
            self._record_failure_trace(result, tool_call_id=tool_call_id, started=started)
            return result

        git_after = collect_git_state(self.workspace_root)
        index_update = self._update_project_index(changeset, transaction_id)
        verification_status = self._run_verification_hook(transaction_id, changeset=changeset)
        result = MutationResult(
            ok=True,
            status="applied",
            message="Mutation transaction applied.",
            changeset_id=changeset.id,
            transaction_id=transaction_id,
            affected_files=changeset.affected_files,
            diffs=changeset.diffs,
            policy_decisions=changeset.policy_decisions,
            observation=self._observation("applied", changeset)
            | {
                "transaction_id": transaction_id,
                "project_index": index_update,
            },
            verification_status=verification_status,
            git_before=git_before,
            git_after=git_after,
        )
        self._changeset_transactions[changeset.id] = transaction_id
        self._remember_changeset_result(result)
        if self.planner is not None:
            self.planner.update_from_mutation(result.observation | {"transaction_id": transaction_id}, tool_call_id=tool_call_id)
        return result

    def _enforce_policy(
        self,
        changeset: ChangeSet,
        transaction_id: str,
    ) -> MutationResult | None:
        request = self._policy_request(changeset, transaction_id=transaction_id)
        decision = self.policy_engine.enforce(request)
        self._record_policy_trace(request, decision)
        if decision.outcome == DecisionOutcome.ALLOW:
            impact_result = self._project_index_policy_result(changeset, transaction_id)
            if impact_result is not None:
                return impact_result
            return None
        self._record_policy_observation(request, decision)
        return MutationResult(
            ok=False,
            status=(
                "requires_review"
                if decision.outcome == DecisionOutcome.REQUIRE_REVIEW
                else decision.outcome.value
            ),
            error_code=_policy_error_code(decision.outcome),
            message=decision.reason,
            changeset_id=changeset.id,
            transaction_id=transaction_id,
            affected_files=changeset.affected_files,
            diffs=changeset.diffs,
            policy_decisions=changeset.policy_decisions,
            observation=self._observation(
                "requires_review"
                if decision.outcome == DecisionOutcome.REQUIRE_REVIEW
                else "rejected",
                changeset,
                error_code=_policy_error_code(decision.outcome),
                error_details={"policy": decision.to_dict(), "request": request.to_dict()},
            ),
        )

    def _rollback_policy_result(
        self,
        *,
        changeset_id: str,
        transaction_id: str,
        reason: str | None,
        changed_files: list[str],
    ) -> MutationResult | None:
        request = PolicyRequest(
            session_id=getattr(self.planner, "session_id", "mutation_session"),
            task_id=getattr(self.planner, "task_id", "mutation_task"),
            phase_id=getattr(getattr(self.planner, "state", None), "current_phase", "mutation"),
            action_id=transaction_id,
            component=PolicyComponent.MUTATION,
            operation=OperationKind.ROLLBACK,
            capability=Capability.ROLLBACK_MUTATION,
            subject=PolicySubject(subject_type="component", name="WorkspaceMutationManager"),
            resource=ResourceRef(
                "changeset",
                changeset_id,
                workspace_relative=True,
                metadata={"files": changed_files},
            ),
            reason=reason or "Rollback workspace changeset.",
            proposed_by_model=False,
            metadata={
                "changeset_id": changeset_id,
                "transaction_id": transaction_id,
                "files_changed": changed_files,
                "reversible": True,
            },
            reversible=True,
            touches_workspace=True,
            workspace_root=str(self.workspace_root),
        )
        decision = self.policy_engine.enforce(request)
        self._record_policy_trace(request, decision)
        if decision.outcome == DecisionOutcome.ALLOW:
            return None
        self._record_policy_observation(request, decision)
        return MutationResult(
            ok=False,
            status="rollback_failed",
            error_code=_policy_error_code(decision.outcome),
            message=decision.reason,
            changeset_id=changeset_id,
            transaction_id=transaction_id,
            affected_files=changed_files,
            observation={
                "mutation_status": "rollback_failed",
                "changeset_id": changeset_id,
                "transaction_id": transaction_id,
                "changed_files": changed_files,
                "diff_summary": [],
                "diff_digest": _digest_diffs([]),
                "artifact_refs": [f"changeset:{changeset_id}"],
                "warnings": [decision.reason],
                "error_code": _policy_error_code(decision.outcome),
            },
        )

    def _policy_request(
        self,
        changeset: ChangeSet,
        *,
        transaction_id: str,
    ) -> PolicyRequest:
        operation = _operation_kind_for_changeset(changeset)
        capability = _capability_for_changeset(changeset)
        resource = ResourceRef(
            resource_type="file" if len(changeset.affected_files) <= 1 else "workspace",
            identifier=changeset.affected_files[0] if len(changeset.affected_files) == 1 else ",".join(changeset.affected_files),
            workspace_relative=True,
            metadata={"files": changeset.affected_files},
        )
        return PolicyRequest(
            session_id=getattr(self.planner, "session_id", "mutation_session"),
            task_id=getattr(self.planner, "task_id", "mutation_task"),
            phase_id=getattr(getattr(self.planner, "state", None), "current_phase", "mutation"),
            action_id=transaction_id,
            component=PolicyComponent.MUTATION,
            operation=operation,
            capability=capability,
            subject=PolicySubject(subject_type="component", name="WorkspaceMutationManager"),
            resource=resource,
            reason=changeset.intent,
            proposed_by_model=True,
            metadata={
                "diff_summary": changeset.preview(),
                "files_changed": changeset.affected_files,
                "project_index_impact": self._project_index_impact(changeset),
                "created": [
                    path
                    for path, snapshot in changeset.base_snapshots.items()
                    if snapshot is None and path in changeset.affected_files
                ],
                "deleted": [
                    path
                    for path, final_text in changeset.final_texts.items()
                    if final_text is None
                ],
                "reversible": True,
                "transaction_id": transaction_id,
                "changeset_id": changeset.id,
            },
            reversible=True,
            touches_workspace=True,
            destructive=operation == OperationKind.DELETE_FILE,
            workspace_root=str(self.workspace_root),
        )

    def _record_policy_observation(
        self,
        request: PolicyRequest,
        decision: Any,
    ) -> None:
        if self.planner is None or not hasattr(self.planner, "record_policy_observation"):
            return
        self.planner.record_policy_observation(
            {
                "outcome": decision.outcome.value,
                "component": request.component.value,
                "operation": request.operation.value,
                "reason": decision.reason,
                "risk_level": decision.risk_level.value,
                "resource": request.resource.identifier,
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
                    "resource": request.resource.identifier,
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

    def _snapshot_for_operation(
        self,
        operation: EditOperation,
        resolved: ResolvedWorkspacePath,
    ) -> FileSnapshot | None:
        if isinstance(operation, CreateFile):
            if resolved.path.exists():
                raise MutationError(
                    "invalid_operation",
                    f"Cannot create file that already exists: {resolved.relative_posix}",
                    {"path": resolved.relative_posix},
                )
            return None
        if isinstance(operation, ApplyUnifiedDiff):
            try:
                file_patch = parse_unified_diff(operation.diff)[0]
            except UnifiedDiffError as exc:
                raise MutationError(exc.code, str(exc), {"path": exc.path or resolved.relative_posix}) from exc
            if file_patch.is_delete:
                raise MutationError(
                    "unsupported_operation",
                    "Deleting files through apply_patch is not supported.",
                    {"path": resolved.relative_posix},
                )
            if file_patch.is_new_file:
                if resolved.path.exists():
                    raise MutationError(
                        "file_changed",
                        "Patch creates a file that already exists.",
                        {"path": resolved.relative_posix},
                    )
                return None
        if not resolved.path.exists():
            raise MutationError(
                "file_not_found",
                f"File does not exist: {resolved.relative_posix}",
                {"path": resolved.relative_posix},
            )
        if not resolved.path.is_file():
            raise MutationError(
                "invalid_operation",
                f"Path is not a file: {resolved.relative_posix}",
                {"path": resolved.relative_posix},
            )
        return FileSnapshot.from_path(resolved.path, relative_path=resolved.relative_posix)

    def _read_text(
        self, resolved: ResolvedWorkspacePath, snapshot: FileSnapshot
    ) -> str:
        if snapshot.is_binary:
            raise MutationError(
                "binary_file_denied",
                f"Binary file cannot be edited: {resolved.relative_posix}",
                {"path": resolved.relative_posix},
            )
        try:
            return resolved.path.read_text(encoding=snapshot.encoding or "utf-8")
        except UnicodeDecodeError as exc:
            raise MutationError(
                "encoding_error",
                f"Could not decode file: {resolved.relative_posix}",
                {"path": resolved.relative_posix},
            ) from exc

    def _assert_expected_snapshot(
        self,
        operation: EditOperation,
        base_snapshots: dict[str, FileSnapshot | None],
        resolved_paths: dict[str, ResolvedWorkspacePath],
    ) -> None:
        expected = operation_expected_sha(operation)
        if expected is None:
            return
        for path in operation_paths(operation):
            resolved = self.resolver.resolve(path)
            snapshot = base_snapshots.get(resolved.relative_posix)
            if snapshot is None or snapshot.sha256 != expected:
                raise MutationError(
                    "snapshot_mismatch",
                    f"Snapshot mismatch for {resolved.relative_posix}",
                    {"path": resolved.relative_posix, "expected_sha256": expected},
                )

    def _apply_operation(
        self,
        operation: EditOperation,
        base_texts: dict[str, str],
        final_texts: dict[str, str | None],
        resolved_paths: dict[str, ResolvedWorkspacePath],
    ) -> None:
        if isinstance(operation, ReplaceText):
            path = self.resolver.resolve(operation.path).relative_posix
            text = self._required_text(path, final_texts)
            count = text.count(operation.old_text)
            if count == 0:
                raise MutationError(
                    "patch_context_not_found",
                    f"Text to replace was not found in {path}",
                    {"path": path},
                )
            if count > 1:
                raise MutationError(
                    "patch_context_ambiguous",
                    f"Text to replace appears multiple times in {path}",
                    {"path": path, "count": count},
                )
            final_texts[path] = text.replace(operation.old_text, operation.new_text, 1)
            return
        if isinstance(operation, InsertBefore):
            path = self.resolver.resolve(operation.path).relative_posix
            text = self._required_text(path, final_texts)
            index = text.find(operation.marker)
            if index == -1:
                raise MutationError("patch_context_not_found", "Insert marker not found.", {"path": path})
            final_texts[path] = text[:index] + operation.text + text[index:]
            return
        if isinstance(operation, InsertAfter):
            path = self.resolver.resolve(operation.path).relative_posix
            text = self._required_text(path, final_texts)
            index = text.find(operation.marker)
            if index == -1:
                raise MutationError("patch_context_not_found", "Insert marker not found.", {"path": path})
            insert_at = index + len(operation.marker)
            final_texts[path] = text[:insert_at] + operation.text + text[insert_at:]
            return
        if isinstance(operation, ReplaceRange):
            path = self.resolver.resolve(operation.path).relative_posix
            text = self._required_text(path, final_texts)
            lines = text.splitlines(keepends=True)
            if operation.start_line < 1 or operation.end_line < operation.start_line:
                raise MutationError("invalid_operation", "Invalid line range.", {"path": path})
            if operation.end_line > len(lines):
                raise MutationError("patch_context_not_found", "Line range exceeds file length.", {"path": path})
            replacement = operation.new_text
            if replacement and not replacement.endswith(("\n", "\r\n")):
                replacement += "\n"
            final_texts[path] = (
                "".join(lines[: operation.start_line - 1])
                + replacement
                + "".join(lines[operation.end_line :])
            )
            return
        if isinstance(operation, ApplyUnifiedDiff):
            path = self.resolver.resolve(operation.path).relative_posix
            current_text = final_texts.get(path)
            try:
                final_texts[path] = apply_unified_diff_to_text(
                    current_text or "",
                    operation.diff,
                    path=path,
                )
            except UnifiedDiffError as exc:
                raise MutationError(exc.code, str(exc), {"path": exc.path or path}) from exc
            return
        if isinstance(operation, CreateFile):
            path = self.resolver.resolve(operation.path).relative_posix
            final_texts[path] = operation.content
            return
        if isinstance(operation, DeleteFile):
            path = self.resolver.resolve(operation.path).relative_posix
            self._required_text(path, final_texts)
            final_texts[path] = None
            return
        if isinstance(operation, MoveFile):
            old_path = self.resolver.resolve(operation.path).relative_posix
            new_path = self.resolver.resolve(operation.new_path).relative_posix
            text = self._required_text(old_path, final_texts)
            final_texts[old_path] = None
            final_texts[new_path] = text
            base_texts.setdefault(new_path, "")
            return
        if isinstance(operation, UpdateJson):
            path = self.resolver.resolve(operation.path).relative_posix
            text = self._required_text(path, final_texts)
            try:
                payload = json.loads(text)
            except json.JSONDecodeError as exc:
                raise MutationError("invalid_operation", "Invalid JSON file.", {"path": path}) from exc
            payload = self._deep_update(payload, operation.updates)
            final_texts[path] = json.dumps(payload, ensure_ascii=False, indent=2) + "\n"
            return
        if isinstance(operation, (UpdateYaml, UpdateToml)):
            raise MutationError(
                "invalid_operation",
                f"{operation_type(operation)} interface is reserved for a structured parser.",
            )
        if isinstance(operation, FormatFile):
            path = self.resolver.resolve(operation.path).relative_posix
            self._required_text(path, final_texts)
            return
        raise MutationError("invalid_operation", f"Unsupported operation: {operation!r}")

    @staticmethod
    def _required_text(path: str, final_texts: dict[str, str | None]) -> str:
        text = final_texts.get(path)
        if text is None:
            raise MutationError("file_not_found", f"File text is unavailable: {path}", {"path": path})
        return text

    def _preflight_current_state(self, changeset: ChangeSet) -> None:
        for path, base_snapshot in changeset.base_snapshots.items():
            current_hash = self.index.current_hash(path)
            if base_snapshot is None and current_hash is not None:
                raise MutationError("file_changed", f"File appeared before write: {path}", {"path": path})
            if base_snapshot is not None and current_hash != base_snapshot.sha256:
                raise MutationError(
                    "file_changed",
                    f"File changed after snapshot: {path}",
                    {"path": path, "expected_sha256": base_snapshot.sha256, "current_sha256": current_hash},
                )

    def _policy_result(self, changeset: ChangeSet) -> MutationResult | None:
        denied = [decision for decision in changeset.policy_decisions if decision.decision == DENY]
        if denied:
            code = denied[0].error_code or "policy_denied"
            return MutationResult(
                ok=False,
                status="rejected",
                error_code=code,
                message="Workspace policy denied mutation.",
                changeset_id=changeset.id,
                affected_files=changeset.affected_files,
                diffs=changeset.diffs,
                policy_decisions=changeset.policy_decisions,
                observation=self._observation("rejected", changeset, error_code=code),
            )
        review = [
            decision
            for decision in changeset.policy_decisions
            if decision.decision == REQUIRE_REVIEW
        ]
        if review:
            return MutationResult(
                ok=False,
                status="requires_review",
                error_code="review_required",
                message="Workspace policy requires review before apply.",
                changeset_id=changeset.id,
                affected_files=changeset.affected_files,
                diffs=changeset.diffs,
                policy_decisions=changeset.policy_decisions,
                observation=self._observation(
                    "requires_review",
                    changeset,
                    error_code="review_required",
                ),
            )
        return None

    def _rollback_entries(self, entries: list[JournalEntry]) -> str | None:
        for entry in reversed(entries):
            try:
                self._rollback_entry(entry)
            except MutationError as exc:
                self._record_rollback_trace(entry, error_code=exc.code, rolled_back=False)
                return exc.code
            self._record_rollback_trace(entry, error_code=None, rolled_back=True)
        return None

    def _rollback_entry(self, entry: JournalEntry) -> None:
        path = self.workspace_root / entry.path
        current_raw = path.read_bytes() if path.exists() else None
        current_sha = hash_bytes(current_raw) if current_raw is not None else None
        if current_sha != entry.after_sha256:
            raise MutationError(
                "rollback_conflict",
                f"File changed after transaction: {entry.path}",
                {"path": entry.path},
            )
        before_rollback_snapshot = (
            FileSnapshot.from_path(path, relative_path=entry.path)
            if path.exists()
            else None
        )
        if entry.before_artifact_path is None:
            if path.exists():
                path.unlink()
        else:
            before_path = self.workspace_root / entry.before_artifact_path
            before_raw = before_path.read_bytes()
            self.atomic_writer.write_bytes(path, before_raw, snapshot=None)
        after_rollback_snapshot = (
            FileSnapshot.from_path(path, relative_path=entry.path)
            if path.exists()
            else None
        )
        if self.workspace_state_manager is not None:
            rollback_id = f"rollback:{entry.operation_id}"
            if hasattr(self.workspace_state_manager, "record_rollback"):
                self.workspace_state_manager.record_rollback(
                    path=entry.path,
                    before_snapshot=before_rollback_snapshot,
                    after_snapshot=after_rollback_snapshot,
                    transaction_id=entry.transaction_id,
                    mutation_id=rollback_id,
                    metadata={"changeset_id": entry.changeset_id, "action": "rollback"},
                )
            else:
                self.workspace_state_manager.record_mutation(
                    path=entry.path,
                    before_snapshot=before_rollback_snapshot,
                    after_snapshot=after_rollback_snapshot,
                    transaction_id=entry.transaction_id,
                    mutation_id=rollback_id,
                    tool_call_id=None,
                    before_bytes=current_raw,
                    metadata={"changeset_id": entry.changeset_id, "action": "rollback"},
                )

    def _record_rollback_trace(
        self,
        entry: JournalEntry,
        *,
        error_code: str | None,
        rolled_back: bool,
    ) -> None:
        if self.trace is None:
            return
        self.trace.record(
            "mutation",
            {
                "transaction_id": entry.transaction_id,
                "changeset_id": entry.changeset_id,
                "operation_id": entry.operation_id,
                "tool_call_id": None,
                "path": entry.path,
                "operation_type": "rollback",
                "policy_decision": None,
                "risk_tags": ["rollback"],
                "before_sha256": entry.after_sha256,
                "after_sha256": entry.before_sha256 if rolled_back else None,
                "diff_digest": None,
                "added_lines": 0,
                "removed_lines": 0,
                "dry_run": False,
                "applied": False,
                "rejected": False,
                "rolled_back": rolled_back,
                "error_code": error_code,
                "duration_ms": 0,
                "artifact_path": entry.before_artifact_path,
                "verification_status": "not_run",
            },
        )

    def _operation_for_path(self, changeset: ChangeSet, path: str) -> EditOperation:
        for operation in changeset.operations:
            paths = [
                self.resolver.resolve(candidate).relative_posix
                for candidate in operation_paths(operation)
            ]
            if path in paths:
                return operation
        return changeset.operations[0]

    def _record_mutation_trace(
        self,
        *,
        changeset: ChangeSet,
        transaction_id: str,
        entry: JournalEntry,
        tool_call_id: str | None,
        status: str,
        error_code: str | None,
        duration_ms: int,
    ) -> None:
        if self.trace is None:
            return
        diff = next((candidate for candidate in changeset.diffs if candidate.path == entry.path), None)
        decision = changeset.policy_decisions[0] if changeset.policy_decisions else None
        self.trace.record(
            "mutation",
            {
                "transaction_id": transaction_id,
                "changeset_id": changeset.id,
                "operation_id": entry.operation_id,
                "tool_call_id": tool_call_id,
                "path": entry.path,
                "operation_type": operation_type(self._operation_for_path(changeset, entry.path)),
                "policy_decision": decision.decision if decision else ALLOW,
                "risk_tags": decision.risk_tags if decision else [],
                "before_sha256": entry.before_sha256,
                "after_sha256": entry.after_sha256,
                "diff_digest": diff.digest if diff else None,
                "added_lines": diff.added_lines if diff else 0,
                "removed_lines": diff.removed_lines if diff else 0,
                "dry_run": status == "preview",
                "applied": status == "applied",
                "rejected": status == "rejected",
                "rolled_back": status == "rolled_back",
                "error_code": error_code,
                "duration_ms": duration_ms,
                "artifact_path": diff.artifact_path if diff else None,
                "verification_status": "not_run",
            },
        )

    def _emit_observability(
        self,
        event_type: TraceEventType,
        *,
        summary: str,
        payload: dict[str, Any],
        transaction_id: str | None,
        action_id: str | None = None,
    ) -> None:
        if self.trace is None or not hasattr(self.trace, "emit"):
            return
        self.trace.emit(
            event_type,
            component="mutation",
            summary=summary,
            payload=payload,
            ids={
                "session_id": getattr(self.planner, "session_id", None),
                "task_id": getattr(self.planner, "task_id", None),
                "phase_id": getattr(getattr(self.planner, "state", None), "current_phase", None),
                "action_id": action_id,
                "transaction_id": transaction_id,
            },
        )

    def _record_failure_trace(
        self,
        result: MutationResult,
        *,
        tool_call_id: str | None,
        started: float,
    ) -> None:
        if self.trace is None:
            return
        path = result.affected_files[0] if result.affected_files else None
        self.trace.record(
            "mutation",
            {
                "transaction_id": result.transaction_id,
                "changeset_id": result.changeset_id,
                "operation_id": result.operation_id,
                "tool_call_id": tool_call_id,
                "path": path,
                "operation_type": None,
                "policy_decision": (
                    result.policy_decisions[0].decision
                    if result.policy_decisions
                    else None
                ),
                "risk_tags": (
                    result.policy_decisions[0].risk_tags
                    if result.policy_decisions
                    else []
                ),
                "before_sha256": None,
                "after_sha256": None,
                "diff_digest": None,
                "added_lines": 0,
                "removed_lines": 0,
                "dry_run": False,
                "applied": False,
                "rejected": True,
                "rolled_back": False,
                "error_code": result.error_code,
                "duration_ms": int((time.perf_counter() - started) * 1000),
                "artifact_path": None,
                "verification_status": result.verification_status,
            },
        )

    def _failure_result(self, exc: MutationError, *, status: str) -> MutationResult:
        return MutationResult(
            ok=False,
            status=status,
            error_code=exc.code,
            message=exc.message,
            observation={
                "mutation_status": "failed",
                "changed_files": [],
                "diff_summary": [],
                "risk_note": exc.message,
                "next_recommended_action": "Fix the operation and retry.",
                "error_code": exc.code,
                "error_details": exc.details,
            },
        )

    def _observation(
        self,
        status: str,
        changeset: ChangeSet,
        *,
        error_code: str | None = None,
        error_details: dict[str, Any] | None = None,
    ) -> dict[str, Any]:
        review_reasons = [
            reason
            for decision in changeset.policy_decisions
            if decision.decision != ALLOW
            for reason in decision.reasons
        ]
        observation = {
            "mutation_status": status,
            "changeset_id": changeset.id,
            "changed_files": changeset.affected_files,
            "diff_summary": [diff.summary() for diff in changeset.diffs],
            "diff_digest": _digest_diffs(changeset.diffs),
            "artifact_refs": self._artifact_refs(ids=[changeset.id], diffs=changeset.diffs),
            "warnings": review_reasons,
            "risk_level": changeset.risk_level,
            "risk_note": "; ".join(review_reasons) if review_reasons else "Policy allowed mutation.",
            "next_recommended_action": (
                "Request review before applying."
                if status == "requires_review"
                else "Run verification hook or project tests."
                if status == "applied"
                else "Fix the mutation request and retry."
            ),
            "error_code": error_code,
            "error_details": error_details,
        }
        return observation

    @staticmethod
    def _risk_level(decisions: list[PolicyDecision], diffs: list[FileDiff]) -> str:
        if any(decision.decision == DENY for decision in decisions):
            return "denied"
        if any(decision.decision == REQUIRE_REVIEW for decision in decisions):
            return "high"
        if any(diff.added_lines + diff.removed_lines > 100 for diff in diffs):
            return "medium"
        return "low"

    @staticmethod
    def _deep_update(payload: Any, updates: dict[str, Any]) -> Any:
        if not isinstance(payload, dict):
            raise MutationError("invalid_operation", "JSON root must be an object.")
        for key, value in updates.items():
            if isinstance(value, dict) and isinstance(payload.get(key), dict):
                payload[key] = WorkspaceMutationManager._deep_update(payload[key], value)
            else:
                payload[key] = value
        return payload

    def _run_verification_hook(self, transaction_id: str, *, changeset: ChangeSet | None = None) -> str:
        if self.verification_hook is None:
            return "not_run"
        try:
            result = self.verification_hook(transaction_id)
        except TypeError:
            try:
                result = self.verification_hook(
                    transaction_id=transaction_id,
                    changeset_id=changeset.id if changeset else None,
                    changed_files=changeset.affected_files if changeset else [],
                    intent=changeset.intent if changeset else "",
                    diff_digests=[diff.digest for diff in changeset.diffs] if changeset else [],
                )
            except Exception:
                return "failed"
        except Exception:
            return "failed"
        return str(result or "completed")

    def _project_index_impact(self, changeset: ChangeSet) -> dict[str, Any]:
        if self.project_index is None:
            return {}
        try:
            return self.project_index.analyze_impact(changeset.affected_files).to_dict()
        except Exception as exc:
            return {"error": type(exc).__name__, "message": str(exc)}

    def _project_index_policy_result(
        self,
        changeset: ChangeSet,
        transaction_id: str,
    ) -> MutationResult | None:
        impact = self._project_index_impact(changeset)
        if not impact or impact.get("error"):
            return None
        if not (
            impact.get("config_impact")
            or impact.get("generated_or_vendor_impact")
            or impact.get("broad_impact")
            or impact.get("affected_entrypoints")
        ):
            return None
        reasons = list(impact.get("risk_reasons") or ["Code index impact requires review."])
        return MutationResult(
            ok=False,
            status="requires_review",
            error_code="approval_required",
            message="Code index impact requires review: " + "; ".join(str(item) for item in reasons),
            changeset_id=changeset.id,
            transaction_id=transaction_id,
            affected_files=changeset.affected_files,
            diffs=changeset.diffs,
            policy_decisions=changeset.policy_decisions,
            observation=self._observation(
                "requires_review",
                changeset,
                error_code="approval_required",
                error_details={"project_index_impact": impact},
            ),
        )

    def _update_project_index(self, changeset: ChangeSet, transaction_id: str) -> dict[str, Any]:
        if self.project_index is None:
            return {}
        try:
            result = self.project_index.update_after_changeset(
                {
                    "changeset_id": changeset.id,
                    "transaction_id": transaction_id,
                    "changed_files": changeset.affected_files,
                    "deleted_files": [
                        path for path, final_text in changeset.final_texts.items() if final_text is None
                    ],
                },
                reason="mutation_applied",
            )
            return result.to_dict() if hasattr(result, "to_dict") else dict(result)
        except Exception as exc:
            return {"error": type(exc).__name__, "message": str(exc)}


class RollbackManager:
    def __init__(self, manager: WorkspaceMutationManager) -> None:
        self.component = manager

    def rollback(self, transaction_id: str) -> MutationResult:
        journal = self.component._journals.get(transaction_id)
        if journal is None:
            return MutationResult(
                ok=False,
                status="rollback_failed",
                error_code="rollback_failed",
                message=f"Unknown transaction: {transaction_id}",
                transaction_id=transaction_id,
            )
        try:
            rollback_error = self.component._rollback_entries(journal.entries)
        except Exception as exc:
            rollback_error = getattr(exc, "code", "rollback_failed")
        if rollback_error:
            return MutationResult(
                ok=False,
                status="rollback_failed",
                error_code=rollback_error,
                message=f"Rollback failed for transaction: {transaction_id}",
                transaction_id=transaction_id,
            )

        return MutationResult(
            ok=True,
            status="rolled_back",
            transaction_id=transaction_id,
            message="Transaction rolled back.",
        )


def normalize_line_endings(text: str, line_ending: str | None) -> str:
    normalized = text.replace("\r\n", "\n").replace("\r", "\n")
    if line_ending == "crlf":
        return normalized.replace("\n", "\r\n")
    return normalized


def _now() -> str:
    return datetime.now(UTC).isoformat()


def _operation_kind_for_changeset(changeset: ChangeSet) -> OperationKind:
    if any(final_text is None for final_text in changeset.final_texts.values()):
        return OperationKind.DELETE_FILE
    if any(path not in changeset.base_snapshots or changeset.base_snapshots[path] is None for path in changeset.affected_files):
        return OperationKind.CREATE_FILE
    return OperationKind.MUTATE_FILE


def _capability_for_changeset(changeset: ChangeSet) -> Capability:
    operation = _operation_kind_for_changeset(changeset)
    if operation == OperationKind.CREATE_FILE:
        return Capability.CREATE_FILE
    if operation == OperationKind.DELETE_FILE:
        return Capability.DELETE_FILE
    return Capability.MUTATE_WORKSPACE


def _policy_error_code(outcome: DecisionOutcome) -> str:
    mapping = {
        DecisionOutcome.DENY: "policy_denied",
        DecisionOutcome.REQUIRE_REVIEW: "approval_required",
        DecisionOutcome.SANDBOX_REQUIRED: "sandbox_required",
        DecisionOutcome.ASK_USER: "policy_ask_user_required",
        DecisionOutcome.ESCALATE: "policy_escalation_required",
    }
    return mapping.get(outcome, "policy_denied")


def _make_unified_diff(path: str, before: str, after: str) -> str:
    return "".join(
        difflib.unified_diff(
            before.splitlines(keepends=True),
            after.splitlines(keepends=True),
            fromfile=f"a/{path}",
            tofile=f"b/{path}",
        )
    )


def _digest_diffs(diffs: list[FileDiff]) -> str:
    return hash_bytes("\n".join(diff.digest for diff in diffs).encode("utf-8"))


def _diff_excerpt(diffs: list[FileDiff], *, limit: int = 12000) -> str:
    lines: list[str] = []
    for diff in diffs:
        lines.append(f"--- a/{diff.path}")
        lines.append(f"+++ b/{diff.path}")
        for hunk in diff.hunks:
            lines.append(hunk.header)
            lines.extend(hunk.lines)
    text = "\n".join(lines)
    redacted = redact(text)
    if not isinstance(redacted, str):
        redacted = str(redacted)
    if len(redacted) <= limit:
        return redacted
    return redacted[:limit] + "\n...[truncated]..."
