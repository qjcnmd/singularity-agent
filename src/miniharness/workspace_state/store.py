from __future__ import annotations

import json
import sqlite3
from pathlib import Path
from threading import RLock
from typing import Any

from miniharness.workspace_state.models import (
    ArtifactRecord,
    ChangeOwnership,
    FileSnapshot,
    JournalEvent,
    RollbackItem,
    WorkspaceBaseline,
)


class WorkspaceJournal:
    def __init__(self, path: Path) -> None:
        self.path = path
        self.path.parent.mkdir(parents=True, exist_ok=True)

    def append(self, event: JournalEvent) -> None:
        with self.path.open("a", encoding="utf-8") as file:
            file.write(event.to_json() + "\n")


class WorkspaceStateStore:
    def __init__(self, workspace_root: Path) -> None:
        self.workspace_root = workspace_root
        self.state_root = workspace_root / ".miniharness"
        self.sessions_root = self.state_root / "sessions"
        self.db_path = self.state_root / "workspace_state.sqlite3"
        self.state_root.mkdir(parents=True, exist_ok=True)
        self.sessions_root.mkdir(parents=True, exist_ok=True)
        self._connection = sqlite3.connect(str(self.db_path))
        self._connection.row_factory = sqlite3.Row
        self._lock = RLock()
        self._init_schema()

    @property
    def connection(self) -> sqlite3.Connection:
        return self._connection

    def session_dir(self, session_id: str) -> Path:
        path = self.sessions_root / session_id
        path.mkdir(parents=True, exist_ok=True)
        return path

    def journal(self, session_id: str) -> WorkspaceJournal:
        return WorkspaceJournal(self.session_dir(session_id) / "journal.jsonl")

    def save_session(
        self,
        *,
        session_id: str,
        task_id: str | None,
        workspace_root: str,
        status: str,
        created_at: str,
        baseline_id: str | None = None,
        metadata: dict[str, Any] | None = None,
    ) -> None:
        with self._lock:
            self._connection.execute(
                """
                insert or replace into sessions(
                    session_id, task_id, baseline_id, workspace_root, status,
                    created_at, closed_at, metadata
                )
                values(
                    ?,
                    ?,
                    coalesce(?, (select baseline_id from sessions where session_id = ?)),
                    ?,
                    ?,
                    coalesce((select created_at from sessions where session_id = ?), ?),
                    (select closed_at from sessions where session_id = ?),
                    ?
                )
                """,
                (
                    session_id,
                    task_id,
                    baseline_id,
                    session_id,
                    workspace_root,
                    status,
                    session_id,
                    created_at,
                    session_id,
                    json.dumps(metadata or {}, ensure_ascii=False, default=str),
                ),
            )
            self._connection.commit()

    def update_session_status(
        self,
        session_id: str,
        *,
        status: str,
        closed_at: str | None,
        metadata: dict[str, Any] | None = None,
    ) -> None:
        with self._lock:
            self._connection.execute(
                """
                update sessions
                set status = ?, closed_at = ?, metadata = coalesce(?, metadata)
                where session_id = ?
                """,
                (
                    status,
                    closed_at,
                    (
                        json.dumps(metadata, ensure_ascii=False, default=str)
                        if metadata is not None
                        else None
                    ),
                    session_id,
                ),
            )
            self._connection.commit()

    def load_session(self, session_id: str) -> dict[str, Any] | None:
        row = self._connection.execute(
            "select * from sessions where session_id = ?",
            (session_id,),
        ).fetchone()
        return _row_to_dict(row) if row else None

    def latest_session(self) -> dict[str, Any] | None:
        row = self._connection.execute(
            "select * from sessions order by created_at desc, session_id desc limit 1"
        ).fetchone()
        return _row_to_dict(row) if row else None

    def open_sessions(self) -> list[dict[str, Any]]:
        rows = self._connection.execute(
            """
            select * from sessions
            where status not in ('closed')
            order by created_at desc, session_id desc
            """
        ).fetchall()
        return [_row_to_dict(row) for row in rows]

    def save_baseline(self, baseline: WorkspaceBaseline) -> None:
        payload = baseline.to_dict()
        with self._lock:
            self._connection.execute(
                """
                insert or replace into baselines(
                    baseline_id, session_id, task_id, workspace_root, created_at,
                    policy_version, snapshot_count, payload
                )
                values(?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    baseline.baseline_id,
                    baseline.session_id,
                    baseline.task_id,
                    baseline.workspace_root,
                    baseline.created_at,
                    baseline.policy_version,
                    len(baseline.snapshots),
                    json.dumps(payload, ensure_ascii=False, default=str),
                ),
            )
            self._connection.execute(
                "update sessions set baseline_id = ? where session_id = ?",
                (baseline.baseline_id, baseline.session_id),
            )
            self._connection.commit()

    def load_baseline(self, session_id: str) -> WorkspaceBaseline | None:
        row = self._connection.execute(
            "select payload from baselines where session_id = ? order by created_at desc limit 1",
            (session_id,),
        ).fetchone()
        if row is None:
            return None
        return WorkspaceBaseline.from_dict(json.loads(row["payload"]))

    def append_event(self, event: JournalEvent) -> None:
        self.journal(event.session_id).append(event)
        with self._lock:
            self._connection.execute(
                """
                insert or replace into events(
                    event_id, session_id, event_type, path, ownership, timestamp,
                    transaction_id, command_id, mutation_id, artifact_id, payload
                )
                values(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    event.event_id,
                    event.session_id,
                    event.event_type,
                    event.path,
                    event.ownership.value if event.ownership else None,
                    event.timestamp,
                    event.transaction_id,
                    event.command_id,
                    event.mutation_id,
                    event.artifact_id,
                    event.to_json(),
                ),
            )
            self._connection.commit()

    def upsert_file_state(
        self,
        *,
        session_id: str,
        path: str,
        snapshot: FileSnapshot | None,
        ownership: ChangeOwnership,
        event_id: str,
        transaction_id: str | None = None,
        command_id: str | None = None,
        mutation_id: str | None = None,
        baseline_snapshot: FileSnapshot | None = None,
        before_snapshot: FileSnapshot | None = None,
        rollback_artifact_path: str | None = None,
        updated_at: str,
    ) -> None:
        current_sha = snapshot.sha256 if snapshot else None
        baseline_sha = baseline_snapshot.sha256 if baseline_snapshot else None
        with self._lock:
            self._connection.execute(
                """
                insert or replace into file_state(
                    session_id, path, snapshot, ownership, last_event_id,
                    transaction_id, command_id, mutation_id, baseline_sha256,
                    current_sha256, before_snapshot, rollback_artifact_path, updated_at
                )
                values(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    session_id,
                    path,
                    json.dumps(snapshot.to_dict(), ensure_ascii=False, default=str)
                    if snapshot
                    else None,
                    ownership.value,
                    event_id,
                    transaction_id,
                    command_id,
                    mutation_id,
                    baseline_sha,
                    current_sha,
                    json.dumps(before_snapshot.to_dict(), ensure_ascii=False, default=str)
                    if before_snapshot
                    else None,
                    rollback_artifact_path,
                    updated_at,
                ),
            )
            self._connection.commit()

    def remove_file_state(self, *, session_id: str, path: str) -> None:
        with self._lock:
            self._connection.execute(
                "delete from file_state where session_id = ? and path = ?",
                (session_id, path),
            )
            self._connection.commit()

    def file_states(self, session_id: str) -> list[dict[str, Any]]:
        rows = self._connection.execute(
            "select * from file_state where session_id = ? order by path",
            (session_id,),
        ).fetchall()
        return [_decode_file_state(row) for row in rows]

    def rollback_items(
        self,
        *,
        session_id: str,
        transaction_id: str | None = None,
    ) -> list[RollbackItem]:
        sql = """
            select * from file_state
            where session_id = ? and ownership = ?
        """
        params: list[Any] = [session_id, ChangeOwnership.AGENT_MUTATION.value]
        if transaction_id is not None:
            sql += " and transaction_id = ?"
            params.append(transaction_id)
        sql += " order by updated_at desc, path desc"
        rows = self._connection.execute(sql, params).fetchall()
        items: list[RollbackItem] = []
        for row in rows:
            decoded = _decode_file_state(row)
            items.append(
                RollbackItem(
                    path=decoded["path"],
                    transaction_id=decoded.get("transaction_id"),
                    mutation_id=decoded.get("mutation_id"),
                    before_snapshot=FileSnapshot.from_dict(decoded.get("before_snapshot")),
                    after_snapshot=FileSnapshot.from_dict(decoded.get("snapshot")),
                    before_artifact_path=decoded.get("rollback_artifact_path"),
                )
            )
        return items

    def save_artifact(self, session_id: str, artifact: ArtifactRecord) -> None:
        with self._lock:
            self._connection.execute(
                """
                insert or replace into artifacts(
                    artifact_id, session_id, kind, path, digest, size, created_at,
                    linked_command_id, linked_transaction_id, linked_verification_id,
                    metadata
                )
                values(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    artifact.artifact_id,
                    session_id,
                    artifact.kind,
                    artifact.path,
                    artifact.digest,
                    artifact.size,
                    artifact.created_at,
                    artifact.linked_command_id,
                    artifact.linked_transaction_id,
                    artifact.linked_verification_id,
                    json.dumps(artifact.metadata, ensure_ascii=False, default=str),
                ),
            )
            self._connection.commit()

    def artifacts(self, session_id: str) -> list[ArtifactRecord]:
        rows = self._connection.execute(
            "select * from artifacts where session_id = ? order by created_at",
            (session_id,),
        ).fetchall()
        return [
            ArtifactRecord(
                artifact_id=row["artifact_id"],
                kind=row["kind"],
                path=row["path"],
                digest=row["digest"],
                size=row["size"],
                created_at=row["created_at"],
                linked_command_id=row["linked_command_id"],
                linked_transaction_id=row["linked_transaction_id"],
                linked_verification_id=row["linked_verification_id"],
                metadata=json.loads(row["metadata"]) if row["metadata"] else {},
            )
            for row in rows
        ]

    def _init_schema(self) -> None:
        self._connection.executescript(
            """
            create table if not exists sessions(
                session_id text primary key,
                task_id text,
                baseline_id text,
                workspace_root text not null,
                status text not null,
                created_at text not null,
                closed_at text,
                metadata text
            );

            create table if not exists baselines(
                baseline_id text primary key,
                session_id text not null,
                task_id text,
                workspace_root text not null,
                created_at text not null,
                policy_version text not null,
                snapshot_count integer not null,
                payload text not null
            );

            create table if not exists file_state(
                session_id text not null,
                path text not null,
                snapshot text,
                ownership text not null,
                last_event_id text not null,
                transaction_id text,
                command_id text,
                mutation_id text,
                baseline_sha256 text,
                current_sha256 text,
                before_snapshot text,
                rollback_artifact_path text,
                updated_at text not null,
                primary key(session_id, path)
            );

            create table if not exists events(
                event_id text primary key,
                session_id text not null,
                event_type text not null,
                path text,
                ownership text,
                timestamp text not null,
                transaction_id text,
                command_id text,
                mutation_id text,
                artifact_id text,
                payload text not null
            );

            create table if not exists artifacts(
                artifact_id text primary key,
                session_id text not null,
                kind text not null,
                path text not null,
                digest text not null,
                size integer not null,
                created_at text not null,
                linked_command_id text,
                linked_transaction_id text,
                linked_verification_id text,
                metadata text
            );
            """
        )
        self._connection.commit()


def _row_to_dict(row: sqlite3.Row) -> dict[str, Any]:
    result = dict(row)
    if result.get("metadata"):
        result["metadata"] = json.loads(result["metadata"])
    else:
        result["metadata"] = {}
    return result


def _decode_file_state(row: sqlite3.Row) -> dict[str, Any]:
    result = dict(row)
    result["snapshot"] = json.loads(result["snapshot"]) if result["snapshot"] else None
    result["before_snapshot"] = (
        json.loads(result["before_snapshot"]) if result["before_snapshot"] else None
    )
    return result
