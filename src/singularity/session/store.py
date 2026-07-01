from __future__ import annotations

import json
import sqlite3
from pathlib import Path
from threading import RLock
from typing import Any
from uuid import uuid4

from singularity.session.models import (
    SessionCheckpoint,
    SessionCheckpointKind,
    SessionDetail,
    SessionLaunch,
    SessionRun,
    SessionRunMode,
    SessionState,
    SessionStatus,
    SessionSummary,
    SessionTimelineEvent,
    normalize_path,
    now_iso,
    session_state_for_status,
)


class SessionStore:
    def __init__(self, workspace_root: Path | str) -> None:
        self.workspace_root = Path(workspace_root).expanduser().resolve(strict=False)
        self.state_root = self.workspace_root / ".singularity"
        self.db_path = self.state_root / "session_index.sqlite3"
        self.state_root.mkdir(parents=True, exist_ok=True)
        self._connection = sqlite3.connect(str(self.db_path), check_same_thread=False)
        self._connection.row_factory = sqlite3.Row
        self._lock = RLock()
        self._init_schema()

    def close(self) -> None:
        with self._lock:
            self._connection.close()

    def create_session(
        self,
        *,
        session_id: str,
        project_root: Path | str,
        user_goal: str,
        task_id: str,
    ) -> SessionSummary:
        created_at = now_iso()
        with self._lock:
            self._connection.execute(
                """
                insert or ignore into sessions(
                    session_id, project_root, user_goal, task_id, status,
                    created_at, updated_at
                )
                values(?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    session_id,
                    normalize_path(project_root),
                    user_goal,
                    task_id,
                    SessionStatus.ACTIVE.value,
                    created_at,
                    created_at,
                ),
            )
            self._connection.commit()
        return self.load_session(session_id) or self._summary_from_values(
            session_id=session_id,
            project_root=normalize_path(project_root),
            user_goal=user_goal,
            task_id=task_id,
            status=SessionStatus.ACTIVE,
            created_at=created_at,
            updated_at=created_at,
        )

    def prepare_launch(
        self,
        *,
        mode: SessionRunMode | str,
        user_goal: str,
        project_root: Path | str,
        requested_session_id: str | None = None,
        run_id: str | None = None,
    ) -> SessionLaunch:
        run_mode = SessionRunMode(mode)
        if run_mode == SessionRunMode.NEW:
            base = uuid4().hex[:12]
            return SessionLaunch(
                session_id=f"session_{base}",
                task_id=f"task_{base}",
                run_id=run_id or f"run_{base}",
                mode=run_mode,
                user_goal=user_goal,
            )

        if not requested_session_id:
            raise KeyError("session id is required for continue/resume")
        summary = self.load_session(requested_session_id)
        if summary is None:
            raise KeyError(requested_session_id)
        recoverable_states = {
            SessionState.ACTIVE,
            SessionState.RECOVERABLE,
            SessionState.NEEDS_REVIEW,
            SessionState.BLOCKED,
        }
        if run_mode == SessionRunMode.RESUME and summary.state not in recoverable_states:
            raise ValueError(
                f"Session {requested_session_id} is {summary.state.value}, not recoverable."
            )
        previous_run = self.load_run(summary.last_run_id) if summary.last_run_id else None
        return SessionLaunch(
            session_id=summary.session_id,
            task_id=summary.task_id,
            run_id=run_id or f"run_{uuid4().hex[:12]}",
            mode=run_mode,
            user_goal=user_goal,
            previous_run_id=summary.last_run_id,
            previous_status=summary.status.value,
            previous_trace_run_dir=previous_run.trace_run_dir if previous_run is not None else None,
        )

    def start_run(
        self,
        *,
        session_id: str,
        run_id: str,
        task_id: str,
        mode: SessionRunMode | str,
        user_goal: str,
        trace_run_dir: Path | str,
    ) -> SessionRun:
        started_at = now_iso()
        run_mode = SessionRunMode(mode)
        with self._lock:
            self._connection.execute(
                """
                insert or replace into runs(
                    run_id, session_id, task_id, mode, user_goal, trace_run_dir,
                    status, started_at, ended_at, final_report_ref, summary
                )
                values(?, ?, ?, ?, ?, ?, ?, ?, null, null, ?)
                """,
                (
                    run_id,
                    session_id,
                    task_id,
                    run_mode.value,
                    user_goal,
                    str(trace_run_dir),
                    SessionStatus.ACTIVE.value,
                    started_at,
                    "{}",
                ),
            )
            self._connection.execute(
                """
                update sessions
                set updated_at = ?, status = ?, last_run_id = ?, last_task_status = ?
                where session_id = ?
                """,
                (started_at, SessionStatus.ACTIVE.value, run_id, "running", session_id),
            )
            self._connection.commit()
        return SessionRun(
            run_id=run_id,
            session_id=session_id,
            task_id=task_id,
            mode=run_mode,
            user_goal=user_goal,
            trace_run_dir=str(trace_run_dir),
            status=SessionStatus.ACTIVE,
            started_at=started_at,
        )

    def finish_run(
        self,
        *,
        run_id: str,
        status: SessionStatus | str,
        final_report_ref: str | None = None,
        summary: dict[str, Any] | None = None,
    ) -> None:
        run_status = SessionStatus(status)
        ended_at = now_iso()
        with self._lock:
            row = self._connection.execute(
                "select session_id from runs where run_id = ?",
                (run_id,),
            ).fetchone()
            self._connection.execute(
                """
                update runs
                set status = ?, ended_at = ?, final_report_ref = ?, summary = ?
                where run_id = ?
                """,
                (
                    run_status.value,
                    ended_at,
                    final_report_ref,
                    json.dumps(summary or {}, ensure_ascii=False, default=str),
                    run_id,
                ),
            )
            if row is not None:
                self._connection.execute(
                    """
                    update sessions
                    set status = ?, updated_at = ?, last_task_status = ?
                    where session_id = ?
                    """,
                    (run_status.value, ended_at, run_status.value, row["session_id"]),
                )
            self._connection.commit()

    def record_checkpoint(
        self,
        *,
        session_id: str,
        run_id: str,
        task_id: str,
        kind: SessionCheckpointKind | str,
        summary: str,
        payload: dict[str, Any] | None = None,
    ) -> SessionCheckpoint:
        checkpoint = SessionCheckpoint(
            checkpoint_id=f"checkpoint_{uuid4().hex[:12]}",
            session_id=session_id,
            run_id=run_id,
            task_id=task_id,
            kind=SessionCheckpointKind(kind),
            summary=summary,
            payload=payload or {},
            created_at=now_iso(),
        )
        with self._lock:
            self._connection.execute(
                """
                insert into checkpoints(
                    checkpoint_id, session_id, run_id, task_id, kind, summary,
                    payload, created_at
                )
                values(?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    checkpoint.checkpoint_id,
                    checkpoint.session_id,
                    checkpoint.run_id,
                    checkpoint.task_id,
                    checkpoint.kind.value,
                    checkpoint.summary,
                    json.dumps(checkpoint.payload, ensure_ascii=False, default=str),
                    checkpoint.created_at,
                ),
            )
            self._connection.commit()
        return checkpoint

    def append_timeline_event(
        self,
        *,
        session_id: str,
        event_type: str,
        summary: str,
        run_id: str | None = None,
        task_id: str | None = None,
        payload: dict[str, Any] | None = None,
    ) -> SessionTimelineEvent:
        event = SessionTimelineEvent(
            event_id=f"session_event_{uuid4().hex[:12]}",
            session_id=session_id,
            run_id=run_id,
            task_id=task_id,
            event_type=event_type,
            summary=summary,
            payload=payload or {},
            created_at=now_iso(),
        )
        with self._lock:
            self._connection.execute(
                """
                insert into timeline(
                    event_id, session_id, run_id, task_id, event_type,
                    summary, payload, created_at
                )
                values(?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    event.event_id,
                    event.session_id,
                    event.run_id,
                    event.task_id,
                    event.event_type,
                    event.summary,
                    json.dumps(event.payload, ensure_ascii=False, default=str),
                    event.created_at,
                ),
            )
            self._connection.commit()
        return event

    def load_session(self, session_id: str) -> SessionSummary | None:
        row = self._connection.execute(
            "select * from sessions where session_id = ?",
            (session_id,),
        ).fetchone()
        return self._summary_from_row(row) if row is not None else None

    def load_run(self, run_id: str | None) -> SessionRun | None:
        if not run_id:
            return None
        row = self._connection.execute(
            "select * from runs where run_id = ?",
            (run_id,),
        ).fetchone()
        return _run_from_row(row) if row is not None else None

    def list_sessions(self) -> list[SessionSummary]:
        rows = self._connection.execute(
            "select * from sessions order by updated_at desc, created_at desc, session_id desc"
        ).fetchall()
        return [self._summary_from_row(row) for row in rows]

    def show_session(self, session_id: str) -> SessionDetail:
        summary = self.load_session(session_id)
        if summary is None:
            raise KeyError(session_id)
        runs = [
            _run_from_row(row)
            for row in self._connection.execute(
                "select * from runs where session_id = ? order by started_at, run_id",
                (session_id,),
            ).fetchall()
        ]
        checkpoints = [
            _checkpoint_from_row(row)
            for row in self._connection.execute(
                "select * from checkpoints where session_id = ? order by created_at, checkpoint_id",
                (session_id,),
            ).fetchall()
        ]
        timeline = [
            _timeline_from_row(row)
            for row in self._connection.execute(
                "select * from timeline where session_id = ? order by created_at, event_id",
                (session_id,),
            ).fetchall()
        ]
        return SessionDetail(
            session=summary,
            runs=runs,
            checkpoints=checkpoints,
            timeline=timeline,
        )

    def _summary_from_row(self, row: sqlite3.Row) -> SessionSummary:
        return self._summary_from_values(
            session_id=str(row["session_id"]),
            project_root=str(row["project_root"]),
            user_goal=str(row["user_goal"]),
            task_id=str(row["task_id"]),
            status=SessionStatus(row["status"]),
            created_at=str(row["created_at"]),
            updated_at=str(row["updated_at"]),
            last_run_id=row["last_run_id"],
            last_task_status=row["last_task_status"],
        )

    @staticmethod
    def _summary_from_values(
        *,
        session_id: str,
        project_root: str,
        user_goal: str,
        task_id: str,
        status: SessionStatus,
        created_at: str,
        updated_at: str,
        last_run_id: str | None = None,
        last_task_status: str | None = None,
    ) -> SessionSummary:
        return SessionSummary(
            session_id=session_id,
            project_root=project_root,
            user_goal=user_goal,
            task_id=task_id,
            status=status,
            state=session_state_for_status(status),
            created_at=created_at,
            updated_at=updated_at,
            last_run_id=last_run_id,
            last_task_status=last_task_status,
            continue_command=f'sg continue {session_id} "<new instruction>"',
            resume_command=f"sg resume {session_id}",
            show_command=f"sg session show {session_id}",
        )

    def _init_schema(self) -> None:
        with self._lock:
            self._connection.executescript(
                """
                create table if not exists sessions(
                    session_id text primary key,
                    project_root text not null,
                    user_goal text not null,
                    task_id text not null,
                    status text not null,
                    created_at text not null,
                    updated_at text not null,
                    last_run_id text,
                    last_task_status text
                );
                create table if not exists runs(
                    run_id text primary key,
                    session_id text not null,
                    task_id text not null,
                    mode text not null,
                    user_goal text not null,
                    trace_run_dir text not null,
                    status text not null,
                    started_at text not null,
                    ended_at text,
                    final_report_ref text,
                    summary text not null default '{}'
                );
                create table if not exists checkpoints(
                    checkpoint_id text primary key,
                    session_id text not null,
                    run_id text not null,
                    task_id text not null,
                    kind text not null,
                    summary text not null,
                    payload text not null,
                    created_at text not null
                );
                create table if not exists timeline(
                    event_id text primary key,
                    session_id text not null,
                    run_id text,
                    task_id text,
                    event_type text not null,
                    summary text not null,
                    payload text not null,
                    created_at text not null
                );
                """
            )
            self._connection.commit()


def _run_from_row(row: sqlite3.Row) -> SessionRun:
    return SessionRun(
        run_id=str(row["run_id"]),
        session_id=str(row["session_id"]),
        task_id=str(row["task_id"]),
        mode=SessionRunMode(row["mode"]),
        user_goal=str(row["user_goal"]),
        trace_run_dir=str(row["trace_run_dir"]),
        status=SessionStatus(row["status"]),
        started_at=str(row["started_at"]),
        ended_at=row["ended_at"],
        final_report_ref=row["final_report_ref"],
        summary=json.loads(row["summary"] or "{}"),
    )


def _checkpoint_from_row(row: sqlite3.Row) -> SessionCheckpoint:
    return SessionCheckpoint(
        checkpoint_id=str(row["checkpoint_id"]),
        session_id=str(row["session_id"]),
        run_id=str(row["run_id"]),
        task_id=str(row["task_id"]),
        kind=SessionCheckpointKind(row["kind"]),
        summary=str(row["summary"]),
        payload=json.loads(row["payload"] or "{}"),
        created_at=str(row["created_at"]),
    )


def _timeline_from_row(row: sqlite3.Row) -> SessionTimelineEvent:
    return SessionTimelineEvent(
        event_id=str(row["event_id"]),
        session_id=str(row["session_id"]),
        run_id=row["run_id"],
        task_id=row["task_id"],
        event_type=str(row["event_type"]),
        summary=str(row["summary"]),
        payload=json.loads(row["payload"] or "{}"),
        created_at=str(row["created_at"]),
    )
