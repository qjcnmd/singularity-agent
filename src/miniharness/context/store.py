from __future__ import annotations

import json
import sqlite3
from contextlib import contextmanager
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from threading import RLock
from typing import Any, Iterator


class ContextVersionConflict(RuntimeError):
    pass


@dataclass(frozen=True)
class ContextReference:
    id: str
    type: str
    path: str | None
    line_start: int | None
    line_end: int | None
    digest: str | None
    observation_id: str


@dataclass(frozen=True)
class ContextSnapshot:
    id: str
    run_id: str
    goal: str
    summary: str
    retained_messages: list[dict[str, Any]]
    known_observation_ids: list[str]
    version: int
    created_at: str


class ObservationStore:
    def __init__(self, db_path: Path | None = None) -> None:
        self.db_path = db_path
        if db_path is not None:
            db_path.parent.mkdir(parents=True, exist_ok=True)
            self._connection = sqlite3.connect(str(db_path))
        else:
            self._connection = sqlite3.connect(":memory:")
        self._connection.row_factory = sqlite3.Row
        self._lock = RLock()
        self._init_schema()

    @property
    def connection(self) -> sqlite3.Connection:
        return self._connection

    def append_message(self, *, run_id: str, message: dict[str, Any]) -> None:
        with self._lock:
            self._ensure_run(run_id)
            seq = self._next_message_seq(run_id)
            payload = json.dumps(message, ensure_ascii=False, default=str)
            self._connection.execute(
                """
                insert into messages(run_id, seq, role, content, payload, tool_call_id, created_at)
                values(?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    run_id,
                    seq,
                    message.get("role"),
                    message.get("content"),
                    payload,
                    message.get("tool_call_id"),
                    self._now(),
                ),
            )
            self._connection.commit()

    def load_messages(self, run_id: str) -> list[dict[str, Any]]:
        rows = self._connection.execute(
            "select payload from messages where run_id = ? order by seq",
            (run_id,),
        ).fetchall()
        return [json.loads(row["payload"]) for row in rows]

    def save_observation(self, observation: Any) -> None:
        with self._lock:
            self._ensure_run(observation.run_id)
            self._connection.execute(
                """
                insert or replace into observations(
                    id, run_id, turn, tool_name, tool_call_id, ok, raw_result, preview,
                    truncated, metadata, created_at, input_tokens, preview_tokens,
                    raw_digest, source_refs, cache_hit, duration_seconds, error_code,
                    tool_version, truncation_reason
                )
                values(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    observation.id,
                    observation.run_id,
                    observation.turn,
                    observation.tool_name,
                    observation.tool_call_id,
                    1 if observation.ok else 0,
                    json.dumps(observation.raw_result, ensure_ascii=False, default=str),
                    observation.preview,
                    1 if observation.truncated else 0,
                    json.dumps(observation.metadata, ensure_ascii=False, default=str),
                    observation.created_at,
                    observation.input_tokens,
                    observation.preview_tokens,
                    observation.raw_digest,
                    json.dumps(
                        [ref.__dict__ for ref in observation.source_refs],
                        ensure_ascii=False,
                        default=str,
                    ),
                    1 if observation.cache_hit else 0,
                    observation.duration_seconds,
                    observation.error_code,
                    observation.tool_version,
                    observation.truncation_reason,
                ),
            )
            for ref in observation.source_refs:
                self.save_reference(ref, commit=False)
            self._connection.commit()

    def get_observation(self, observation_id: str) -> Any | None:
        from miniharness.context.manager import ToolObservation

        row = self._connection.execute(
            "select * from observations where id = ?",
            (observation_id,),
        ).fetchone()
        if row is None:
            return None
        refs = self.references_for_observation(observation_id)
        return ToolObservation(
            id=row["id"],
            run_id=row["run_id"],
            turn=row["turn"],
            tool_name=row["tool_name"],
            tool_call_id=row["tool_call_id"],
            ok=bool(row["ok"]),
            raw_result=json.loads(row["raw_result"]) if row["raw_result"] else {},
            preview=row["preview"] or "",
            truncated=bool(row["truncated"]),
            metadata=json.loads(row["metadata"]) if row["metadata"] else {},
            created_at=row["created_at"],
            input_tokens=row["input_tokens"] or 0,
            preview_tokens=row["preview_tokens"] or 0,
            raw_digest=row["raw_digest"] or "",
            source_refs=refs,
            cache_hit=bool(row["cache_hit"]),
            duration_seconds=row["duration_seconds"],
            error_code=row["error_code"],
            tool_version=row["tool_version"],
            truncation_reason=row["truncation_reason"],
        )

    def observation_count(self, run_id: str) -> int:
        row = self._connection.execute(
            "select count(*) as count from observations where run_id = ?",
            (run_id,),
        ).fetchone()
        return int(row["count"])

    def save_reference(self, reference: ContextReference, *, commit: bool = True) -> None:
        self._connection.execute(
            """
            insert or replace into context_references(
                id, observation_id, type, path, line_start, line_end, digest
            )
            values(?, ?, ?, ?, ?, ?, ?)
            """,
            (
                reference.id,
                reference.observation_id,
                reference.type,
                reference.path,
                reference.line_start,
                reference.line_end,
                reference.digest,
            ),
        )
        if commit:
            self._connection.commit()

    def references_for_observation(self, observation_id: str) -> list[ContextReference]:
        rows = self._connection.execute(
            "select * from context_references where observation_id = ? order by id",
            (observation_id,),
        ).fetchall()
        return [
            ContextReference(
                id=row["id"],
                type=row["type"],
                path=row["path"],
                line_start=row["line_start"],
                line_end=row["line_end"],
                digest=row["digest"],
                observation_id=row["observation_id"],
            )
            for row in rows
        ]

    def save_snapshot(self, snapshot: ContextSnapshot) -> None:
        with self._lock:
            self._ensure_run(snapshot.run_id)
            self._connection.execute(
                """
                insert or replace into snapshots(
                    id, run_id, goal, summary, retained_messages,
                    known_observation_ids, version, created_at
                )
                values(?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    snapshot.id,
                    snapshot.run_id,
                    snapshot.goal,
                    snapshot.summary,
                    json.dumps(
                        snapshot.retained_messages, ensure_ascii=False, default=str
                    ),
                    json.dumps(snapshot.known_observation_ids, ensure_ascii=False),
                    snapshot.version,
                    snapshot.created_at,
                ),
            )
            self._connection.commit()

    def latest_snapshot(self, run_id: str) -> ContextSnapshot | None:
        row = self._connection.execute(
            "select * from snapshots where run_id = ? order by created_at desc, id desc limit 1",
            (run_id,),
        ).fetchone()
        if row is None:
            return None
        return ContextSnapshot(
            id=row["id"],
            run_id=row["run_id"],
            goal=row["goal"],
            summary=row["summary"],
            retained_messages=json.loads(row["retained_messages"]),
            known_observation_ids=json.loads(row["known_observation_ids"]),
            version=row["version"],
            created_at=row["created_at"],
        )

    def current_version(self, run_id: str) -> int:
        with self._lock:
            self._ensure_run(run_id)
            self._connection.commit()
            row = self._connection.execute(
                "select version from runs where run_id = ?",
                (run_id,),
            ).fetchone()
            return int(row["version"])

    def bump_version(self, run_id: str, *, expected_version: int) -> int:
        with self.transaction(run_id, expected_version=expected_version):
            pass
        return self.current_version(run_id)

    @contextmanager
    def transaction(self, run_id: str, *, expected_version: int) -> Iterator[None]:
        with self._lock:
            self._connection.execute("begin immediate")
            try:
                self._ensure_run(run_id)
                row = self._connection.execute(
                    "select version from runs where run_id = ?",
                    (run_id,),
                ).fetchone()
                current = int(row["version"])
                if current != expected_version:
                    raise ContextVersionConflict(
                        f"Context version conflict for {run_id}: expected {expected_version}, got {current}"
                    )
                yield
                self._connection.execute(
                    "update runs set version = ? where run_id = ?",
                    (current + 1, run_id),
                )
                self._connection.commit()
            except Exception:
                self._connection.rollback()
                raise

    def _init_schema(self) -> None:
        self._connection.executescript(
            """
            create table if not exists runs(
                run_id text primary key,
                version integer not null default 0,
                created_at text not null
            );

            create table if not exists messages(
                id integer primary key autoincrement,
                run_id text not null,
                seq integer not null,
                role text,
                content text,
                payload text not null,
                tool_call_id text,
                created_at text not null
            );

            create table if not exists observations(
                id text primary key,
                run_id text,
                turn integer,
                tool_name text,
                tool_call_id text,
                ok integer,
                raw_result text,
                preview text,
                truncated integer,
                metadata text,
                created_at text,
                input_tokens integer,
                preview_tokens integer,
                raw_digest text,
                source_refs text,
                cache_hit integer,
                duration_seconds real,
                error_code text,
                tool_version text,
                truncation_reason text
            );

            create table if not exists context_references(
                id text primary key,
                observation_id text not null,
                type text not null,
                path text,
                line_start integer,
                line_end integer,
                digest text
            );

            create table if not exists snapshots(
                id text primary key,
                run_id text not null,
                goal text not null,
                summary text not null,
                retained_messages text not null,
                known_observation_ids text not null,
                version integer not null,
                created_at text not null
            );
            """
        )
        self._connection.commit()

    def _ensure_run(self, run_id: str) -> None:
        self._connection.execute(
            """
            insert or ignore into runs(run_id, version, created_at)
            values(?, 0, ?)
            """,
            (run_id, self._now()),
        )

    def _next_message_seq(self, run_id: str) -> int:
        row = self._connection.execute(
            "select coalesce(max(seq), -1) + 1 as next_seq from messages where run_id = ?",
            (run_id,),
        ).fetchone()
        return int(row["next_seq"])

    @staticmethod
    def _now() -> str:
        return datetime.now(UTC).isoformat()
