from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
import json
import shutil
from typing import Any


@dataclass(frozen=True)
class RecoveryReport:
    recovered: bool
    stale_lock_detected: bool = False
    incomplete_trace_spans: list[str] = field(default_factory=list)
    workspace_recovery: dict[str, Any] = field(default_factory=dict)
    unfinished_mutations: list[str] = field(default_factory=list)
    leftover_sandboxes: list[str] = field(default_factory=list)
    process_records: list[str] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {
            "recovered": self.recovered,
            "stale_lock_detected": self.stale_lock_detected,
            "incomplete_trace_spans": self.incomplete_trace_spans,
            "workspace_recovery": self.workspace_recovery,
            "unfinished_mutations": self.unfinished_mutations,
            "leftover_sandboxes": self.leftover_sandboxes,
            "process_records": self.process_records,
        }


class CrashRecoveryManager:
    def __init__(
        self,
        *,
        trace: Any | None = None,
        workspace_lock: Any | None = None,
        workspace_state: Any | None = None,
        sandbox: Any | None = None,
        command: Any | None = None,
    ) -> None:
        self.trace = trace
        self.workspace_lock = workspace_lock
        self.workspace_state = workspace_state
        self.sandbox = sandbox
        self.command = command

    def recover(self) -> RecoveryReport:
        stale_lock = bool(
            self.workspace_lock is not None
            and (
                bool(getattr(self.workspace_lock, "last_stale_lock_detected", False))
                or (
                    hasattr(self.workspace_lock, "detect_stale_lock")
                    and self.workspace_lock.detect_stale_lock()
                )
            )
        )
        self._record("recovery.detected", {"stale_lock_detected": stale_lock})
        incomplete_spans: list[str] = []
        store = getattr(self.trace, "store", None)
        if store is not None and hasattr(store, "recover_incomplete_spans"):
            incomplete_spans = list(store.recover_incomplete_spans())
        workspace_recovery: dict[str, Any] = {}
        if self.workspace_state is not None and hasattr(self.workspace_state, "recover_session"):
            result = self.workspace_state.recover_session()
            workspace_recovery = result.to_dict() if hasattr(result, "to_dict") else dict(result)
        unfinished_mutations = sorted(
            set(
                [
                    *list(workspace_recovery.get("incomplete_transactions") or []),
                    *self._unfinished_mutation_journals(),
                ]
            )
        )
        leftover_sandboxes = self._leftover_sandboxes()
        process_records = self._process_records()
        self._mark_mutations_recovered(unfinished_mutations)
        self._cleanup_sandboxes(leftover_sandboxes)
        self._stop_processes(process_records)
        report = RecoveryReport(
            recovered=(
                stale_lock
                or bool(incomplete_spans)
                or _workspace_recovered(workspace_recovery)
                or bool(unfinished_mutations)
                or bool(leftover_sandboxes)
                or bool(process_records)
            ),
            stale_lock_detected=stale_lock,
            incomplete_trace_spans=incomplete_spans,
            workspace_recovery=workspace_recovery,
            unfinished_mutations=unfinished_mutations,
            leftover_sandboxes=leftover_sandboxes,
            process_records=process_records,
        )
        self._record("recovery.completed", report.to_dict())
        return report

    def _record(self, event: str, payload: dict[str, Any]) -> None:
        if self.trace is not None and hasattr(self.trace, "record"):
            self.trace.record(event, payload)

    def _unfinished_mutation_journals(self) -> list[str]:
        roots = []
        workspace_root = _workspace_root(self.workspace_state) or _workspace_root(self.sandbox)
        if workspace_root is not None:
            roots.append(workspace_root / ".singularity" / "journals")
        results: list[str] = []
        for root in roots:
            if not root.exists():
                continue
            for journal_dir in root.iterdir():
                if journal_dir.is_dir() and (journal_dir / "journal.jsonl").exists():
                    results.append(journal_dir.name)
        return sorted(results)

    def _leftover_sandboxes(self) -> list[str]:
        workspace_root = _workspace_root(self.sandbox) or _workspace_root(self.workspace_state)
        if workspace_root is None:
            return []
        candidates = [
            workspace_root / "work" / "sandboxes",
            workspace_root / ".singularity" / "sandboxes",
        ]
        leftovers: list[str] = []
        for root in candidates:
            if not root.exists():
                continue
            leftovers.extend(str(path) for path in sorted(root.iterdir()) if path.is_dir())
        return leftovers

    def _process_records(self) -> list[str]:
        if self.command is None or not hasattr(self.command, "list_processes"):
            return []
        records = []
        for process in self.command.list_processes():
            status = str(getattr(process, "status", ""))
            if status == "running":
                records.append(str(getattr(process, "process_id", "")))
        return [record for record in records if record]

    def _mark_mutations_recovered(self, transaction_ids: list[str]) -> None:
        workspace_root = _workspace_root(self.workspace_state) or _workspace_root(self.sandbox)
        if workspace_root is None:
            return
        journals_root = workspace_root / ".singularity" / "journals"
        for transaction_id in transaction_ids:
            journal_dir = journals_root / transaction_id
            if not journal_dir.exists():
                continue
            marker = journal_dir / "recovered.json"
            marker.write_text(
                json.dumps(
                    {
                        "transaction_id": transaction_id,
                        "status": "recovered",
                    },
                    ensure_ascii=False,
                    sort_keys=True,
                ),
                encoding="utf-8",
            )

    def _cleanup_sandboxes(self, sandbox_paths: list[str]) -> None:
        for sandbox_path in sandbox_paths:
            try:
                shutil.rmtree(sandbox_path)
            except FileNotFoundError:
                continue

    def _stop_processes(self, process_ids: list[str]) -> None:
        if self.command is None or not hasattr(self.command, "stop_process"):
            return
        for process_id in process_ids:
            self.command.stop_process(process_id)


def _workspace_root(component: Any | None) -> Path | None:
    root = getattr(component, "workspace_root", None)
    return Path(root) if root is not None else None


def _workspace_recovered(payload: dict[str, Any]) -> bool:
    status = str(payload.get("status") or "")
    return bool(payload) and status not in {"", "clean"}
