from __future__ import annotations

import json
import os
import time
from contextlib import contextmanager
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from singularity.planner.models import (
    EvidenceLedger,
    ExecutionBudget,
    FinalReport,
    TaskPlan,
    TaskState,
)


class PlannerStore:
    def __init__(self, workspace_root: Path | str) -> None:
        self.workspace_root = Path(workspace_root).resolve(strict=False)

    def session_dir(self, session_id: str) -> Path:
        path = self.workspace_root / ".singularity" / "planner" / session_id
        path.mkdir(parents=True, exist_ok=True)
        return path

    def save(
        self,
        *,
        state: TaskState,
        plan: TaskPlan,
        evidence: EvidenceLedger,
        budget: ExecutionBudget,
        final_report: FinalReport | None = None,
    ) -> None:
        session_dir = self.session_dir(state.session_id)
        self._write_json(session_dir / "state.json", state.to_dict())
        self._write_json(session_dir / "plan.json", plan.to_dict())
        self._write_json(session_dir / "evidence.json", evidence.to_dict())
        self._write_json(session_dir / "budget.json", budget.to_dict())
        if final_report is not None:
            self._write_json(session_dir / "final_report.json", final_report.to_dict())

    def load(
        self, session_id: str
    ) -> tuple[TaskState, TaskPlan, EvidenceLedger, ExecutionBudget, FinalReport | None]:
        session_dir = self.session_dir(session_id)
        state = TaskState.from_dict(self._read_json(session_dir / "state.json"))
        plan = TaskPlan.from_dict(self._read_json(session_dir / "plan.json"))
        evidence = EvidenceLedger.from_dict(self._read_json(session_dir / "evidence.json"))
        budget = ExecutionBudget.from_dict(self._read_json(session_dir / "budget.json"))
        final_report_path = session_dir / "final_report.json"
        final_report = (
            FinalReport.from_dict(self._read_json(final_report_path))
            if final_report_path.exists()
            else None
        )
        return state, plan, evidence, budget, final_report

    def append_event(
        self,
        session_id: str,
        *,
        task_id: str,
        phase: str,
        action_id: str | None = None,
        action_kind: str | None = None,
        decision: str | None = None,
        reason: str | None = None,
        evidence_refs: list[str] | None = None,
        budget_state: dict[str, Any] | None = None,
        risk_level: str | None = None,
        replan_decision: dict[str, Any] | None = None,
        completion_assessment: dict[str, Any] | None = None,
        extra: dict[str, Any] | None = None,
    ) -> None:
        payload = {
            "ts": datetime.now(UTC).isoformat(),
            "event": "planner",
            "task_id": task_id,
            "session_id": session_id,
            "phase": phase,
            "action_id": action_id,
            "action_kind": action_kind,
            "decision": decision,
            "reason": reason,
            "evidence_refs": evidence_refs or [],
            "budget_state": budget_state or {},
            "risk_level": risk_level,
            "replan_decision": replan_decision,
            "completion_assessment": completion_assessment,
        }
        payload.update(extra or {})
        path = self.session_dir(session_id) / "planner_events.jsonl"
        line = json.dumps(payload, ensure_ascii=False, default=str) + "\n"
        with _file_lock(path) as handle:
            handle.seek(0, os.SEEK_END)
            handle.write(line.encode("utf-8"))
            handle.flush()

    @staticmethod
    def _write_json(path: Path, payload: dict[str, Any]) -> None:
        text = json.dumps(payload, ensure_ascii=False, indent=2, default=str) + "\n"
        _atomic_write(path, text)

    @staticmethod
    def _read_json(path: Path) -> dict[str, Any]:
        return json.loads(path.read_text(encoding="utf-8"))


@contextmanager
def _file_lock(path: Path):
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a+b") as handle:
        _lock_file(handle)
        try:
            yield handle
        finally:
            _unlock_file(handle)


def _lock_file(handle: Any) -> None:
    if os.name == "nt":
        import msvcrt

        handle.seek(0)
        msvcrt.locking(handle.fileno(), msvcrt.LK_LOCK, 1)
        return
    import fcntl

    fcntl.flock(handle.fileno(), fcntl.LOCK_EX)


def _unlock_file(handle: Any) -> None:
    if os.name == "nt":
        import msvcrt

        handle.seek(0)
        msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)
        return
    import fcntl

    fcntl.flock(handle.fileno(), fcntl.LOCK_UN)


def _atomic_write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    with tmp.open("w", encoding="utf-8", newline="\n") as handle:
        handle.write(text)
        handle.flush()
        os.fsync(handle.fileno())
    _replace_with_retry(tmp, path)
    try:
        directory_fd = os.open(str(path.parent), os.O_RDONLY)
    except OSError:
        return
    try:
        os.fsync(directory_fd)
    except OSError:
        pass
    finally:
        os.close(directory_fd)


def _replace_with_retry(tmp: Path, path: Path) -> None:
    last_error: PermissionError | None = None
    for attempt in range(8):
        try:
            os.replace(tmp, path)
            return
        except PermissionError as exc:
            last_error = exc
            time.sleep(0.05 * (attempt + 1))
    if last_error is not None:
        raise last_error
