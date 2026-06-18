from __future__ import annotations

import json
import sqlite3
from contextlib import contextmanager
from dataclasses import replace
from datetime import UTC, datetime
from pathlib import Path
from threading import RLock
from typing import Any, Iterator

from miniharness.context.models import (
    ContextBundle,
    ContextFreshness,
    ContextItem,
    ContextReference,
    ContextSensitivity,
    ContextSnapshot,
    ToolObservation,
    digest_value,
)
from miniharness.context.redaction import ContextRedactor, SensitivityClassifier


class ContextVersionConflict(RuntimeError):
    pass


class ObservationStore:
    def __init__(
        self,
        db_path: Path | None = None,
        *,
        allow_raw_secret_storage: bool = False,
        redactor: ContextRedactor | None = None,
        trace: Any | None = None,
    ) -> None:
        self.db_path = db_path
        self.allow_raw_secret_storage = allow_raw_secret_storage
        self.redactor = redactor or ContextRedactor()
        self.classifier = SensitivityClassifier()
        self.trace = trace
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

    def append_item(
        self,
        item: ContextItem,
        *,
        expected_version: int | None = None,
    ) -> ContextItem:
        with self._lock:
            self._connection.execute("begin immediate")
            try:
                self._ensure_run(item.run_id)
                current_version = self._run_version(item.run_id)
                if expected_version is not None and current_version != expected_version:
                    raise ContextVersionConflict(
                        f"Context version conflict for {item.run_id}: expected {expected_version}, got {current_version}"
                    )
                stored_item = self._sanitize_item_for_storage(item)
                self._insert_context_item(stored_item)
                for reference in stored_item.references:
                    self._insert_reference(reference)
                self._append_event(
                    stored_item.run_id,
                    event_type="context.item_added",
                    item_id=stored_item.item_id,
                    payload={
                        "item_type": stored_item.item_type.value,
                        "layer": stored_item.layer.value,
                        "source_runtime": stored_item.source_runtime.value,
                        "content_digest": stored_item.content_digest,
                        "sensitivity": stored_item.sensitivity.value,
                    },
                )
                self._set_run_version(item.run_id, current_version + 1)
                self._connection.commit()
            except Exception:
                self._connection.rollback()
                raise
        self._emit_trace(
            "context.item_added",
            {
                "run_id": stored_item.run_id,
                "item_id": stored_item.item_id,
                "item_type": stored_item.item_type.value,
                "content_digest": stored_item.content_digest,
            },
        )
        return stored_item

    def load_item(self, item_id: str) -> ContextItem | None:
        row = self._connection.execute(
            "select * from context_items where item_id = ?",
            (item_id,),
        ).fetchone()
        return self._item_from_row(row) if row is not None else None

    def query_items(
        self,
        *,
        run_id: str | None = None,
        task_id: str | None = None,
        phase_id: str | None = None,
        layer: Any | None = None,
        item_type: Any | None = None,
        source_runtime: Any | None = None,
        freshness: Any | None = None,
    ) -> list[ContextItem]:
        clauses: list[str] = []
        params: list[Any] = []
        for column, value in (
            ("run_id", run_id),
            ("task_id", task_id),
            ("phase_id", phase_id),
            ("layer", _value(layer)),
            ("item_type", _value(item_type)),
            ("source_runtime", _value(source_runtime)),
            ("freshness", _value(freshness)),
        ):
            if value is not None:
                clauses.append(f"{column} = ?")
                params.append(value)
        sql = "select * from context_items"
        if clauses:
            sql += " where " + " and ".join(clauses)
        sql += " order by seq, created_at, item_id"
        rows = self._connection.execute(sql, params).fetchall()
        return [self._item_from_row(row) for row in rows]

    def mark_stale(self, item_id: str, *, reason: str = "") -> None:
        self._update_item_freshness(
            item_id,
            freshness=ContextFreshness.STALE,
            event_type="context.item_stale",
            reason=reason,
        )

    def supersede_item(self, item_id: str, *, superseded_by: str) -> None:
        row = self._connection.execute(
            "select run_id, metadata from context_items where item_id = ?",
            (item_id,),
        ).fetchone()
        if row is None:
            return
        metadata = json.loads(row["metadata"] or "{}")
        metadata["superseded_by"] = superseded_by
        self._connection.execute(
            """
            update context_items
            set freshness = ?, metadata = ?, updated_at = ?
            where item_id = ?
            """,
            (
                ContextFreshness.OBSOLETE.value,
                json.dumps(metadata, ensure_ascii=False, default=str),
                self._now(),
                item_id,
            ),
        )
        self._append_event(
            row["run_id"],
            event_type="context.item_superseded",
            item_id=item_id,
            payload={"superseded_by": superseded_by},
        )
        self._connection.commit()

    def set_item_pinned(self, item_id: str, *, pinned: bool = True) -> None:
        row = self._connection.execute(
            "select run_id from context_items where item_id = ?",
            (item_id,),
        ).fetchone()
        if row is None:
            return
        self._connection.execute(
            """
            update context_items
            set pinned = ?, updated_at = ?
            where item_id = ?
            """,
            (1 if pinned else 0, self._now(), item_id),
        )
        self._append_event(
            row["run_id"],
            event_type="context.item_pinned" if pinned else "context.item_unpinned",
            item_id=item_id,
            payload={"pinned": pinned},
        )
        self._connection.commit()

    def compact_items(
        self,
        *,
        run_id: str,
        omitted_item_ids: list[str],
        summary_item_id: str,
    ) -> None:
        with self._lock:
            self._connection.execute("begin immediate")
            try:
                for item_id in omitted_item_ids:
                    self._connection.execute(
                        """
                        update context_items
                        set freshness = ?, updated_at = ?
                        where item_id = ? and run_id = ?
                        """,
                        (ContextFreshness.STALE.value, self._now(), item_id, run_id),
                    )
                self._append_event(
                    run_id,
                    event_type="context.compaction_completed",
                    item_id=summary_item_id,
                    payload={
                        "summary_item_id": summary_item_id,
                        "omitted_item_ids": omitted_item_ids,
                    },
                )
                self._connection.commit()
            except Exception:
                self._connection.rollback()
                raise

    def events_for_run(self, run_id: str) -> list[dict[str, Any]]:
        rows = self._connection.execute(
            "select * from context_events where run_id = ? order by seq",
            (run_id,),
        ).fetchall()
        return [
            {
                "seq": row["seq"],
                "run_id": row["run_id"],
                "event_type": row["event_type"],
                "item_id": row["item_id"],
                "payload": json.loads(row["payload"] or "{}"),
                "created_at": row["created_at"],
            }
            for row in rows
        ]

    def save_observation(self, observation: ToolObservation) -> None:
        with self._lock:
            self._ensure_run(observation.run_id)
            raw_result = observation.raw_result
            preview = observation.preview
            sensitivity = self.classifier.classify(raw_result)
            if (
                sensitivity in {ContextSensitivity.SECRET, ContextSensitivity.SENSITIVE}
                and not self.allow_raw_secret_storage
            ):
                raw_result = self.redactor.redact_value(raw_result)
                preview = self.redactor.redact_text(preview)
            self._connection.execute(
                """
                insert or replace into observations(
                    id, run_id, turn, tool_name, tool_call_id, ok, raw_result, preview,
                    truncated, metadata, created_at, input_tokens, preview_tokens,
                    raw_digest, source_refs, cache_hit, duration_seconds, error_code,
                    tool_version, truncation_reason, sensitivity
                )
                values(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    observation.id,
                    observation.run_id,
                    observation.turn,
                    observation.tool_name,
                    observation.tool_call_id,
                    1 if observation.ok else 0,
                    json.dumps(raw_result, ensure_ascii=False, default=str),
                    preview,
                    1 if observation.truncated else 0,
                    json.dumps(observation.metadata, ensure_ascii=False, default=str),
                    observation.created_at,
                    observation.input_tokens,
                    observation.preview_tokens,
                    observation.raw_digest,
                    json.dumps(
                        [ref.to_dict() for ref in observation.source_refs],
                        ensure_ascii=False,
                        default=str,
                    ),
                    1 if observation.cache_hit else 0,
                    observation.duration_seconds,
                    observation.error_code,
                    observation.tool_version,
                    observation.truncation_reason,
                    sensitivity.value,
                ),
            )
            for ref in observation.source_refs:
                self._insert_reference(ref)
            self._connection.commit()

    def get_observation(self, observation_id: str) -> ToolObservation | None:
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
            created_at=row["created_at"] or "",
            input_tokens=row["input_tokens"] or 0,
            preview_tokens=row["preview_tokens"] or 0,
            raw_digest=row["raw_digest"] or "",
            source_refs=refs,
            cache_hit=bool(row["cache_hit"]),
            duration_seconds=row["duration_seconds"],
            error_code=row["error_code"],
            tool_version=row["tool_version"],
            truncation_reason=row["truncation_reason"],
            sensitivity=row["sensitivity"] or ContextSensitivity.WORKSPACE,
        )

    def observation_count(self, run_id: str) -> int:
        row = self._connection.execute(
            "select count(*) as count from observations where run_id = ?",
            (run_id,),
        ).fetchone()
        return int(row["count"])

    def save_reference(self, reference: ContextReference, *, commit: bool = True) -> None:
        self._insert_reference(reference)
        if commit:
            self._connection.commit()

    def resolve_reference(self, ref_id: str) -> ContextReference | None:
        row = self._connection.execute(
            "select * from context_references where id = ?",
            (ref_id,),
        ).fetchone()
        return self._reference_from_row(row) if row is not None else None

    def references_for_observation(self, observation_id: str) -> list[ContextReference]:
        rows = self._connection.execute(
            """
            select * from context_references
            where observation_id = ? or source_item_id = ?
            order by id
            """,
            (observation_id, observation_id),
        ).fetchall()
        return [self._reference_from_row(row) for row in rows]

    def references_for_target(
        self,
        target: str,
        *,
        ref_type: str | None = None,
    ) -> list[ContextReference]:
        clauses = ["(target = ? or path = ?)"]
        params: list[Any] = [target, target]
        if ref_type is not None:
            clauses.append("ref_type = ?")
            params.append(ref_type)
        rows = self._connection.execute(
            "select * from context_references where " + " and ".join(clauses) + " order by id",
            params,
        ).fetchall()
        return [self._reference_from_row(row) for row in rows]

    def update_reference_freshness(
        self,
        ref_id: str,
        freshness: ContextFreshness | str,
        *,
        reason: str = "",
    ) -> None:
        freshness_value = _value(freshness)
        self._connection.execute(
            """
            update context_references
            set freshness = ?, metadata = json_set(coalesce(metadata, '{}'), '$.stale_reason', ?)
            where id = ?
            """,
            (freshness_value, reason, ref_id),
        )
        self._connection.commit()

    def save_snapshot(self, snapshot: ContextSnapshot) -> None:
        with self._lock:
            self._ensure_run(snapshot.run_id)
            self._connection.execute(
                """
                insert or replace into context_snapshots(
                    snapshot_id, run_id, session_id, task_id, goal, summary,
                    retained_item_ids, retained_messages, known_observation_ids,
                    version, created_at, metadata
                )
                values(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    snapshot.snapshot_id,
                    snapshot.run_id,
                    snapshot.session_id,
                    snapshot.task_id,
                    snapshot.goal,
                    snapshot.summary,
                    json.dumps(snapshot.retained_item_ids, ensure_ascii=False),
                    json.dumps(snapshot.retained_messages, ensure_ascii=False, default=str),
                    json.dumps(snapshot.known_observation_ids, ensure_ascii=False),
                    snapshot.version,
                    snapshot.created_at,
                    json.dumps(snapshot.metadata, ensure_ascii=False, default=str),
                ),
            )
            self._append_event(
                snapshot.run_id,
                event_type="context.snapshot_saved",
                item_id=snapshot.snapshot_id,
                payload={
                    "known_observation_ids": snapshot.known_observation_ids,
                    "summary_digest": digest_value(snapshot.summary),
                },
            )
            self._connection.commit()

    def latest_snapshot(self, run_id: str) -> ContextSnapshot | None:
        row = self._connection.execute(
            """
            select * from context_snapshots
            where run_id = ?
            order by created_at desc, snapshot_id desc
            limit 1
            """,
            (run_id,),
        ).fetchone()
        if row is None:
            return None
        return ContextSnapshot(
            snapshot_id=row["snapshot_id"],
            run_id=row["run_id"],
            session_id=row["session_id"] or row["run_id"],
            task_id=row["task_id"] or row["run_id"],
            goal=row["goal"] or "",
            summary=row["summary"] or "",
            retained_item_ids=json.loads(row["retained_item_ids"] or "[]"),
            retained_messages=json.loads(row["retained_messages"] or "[]"),
            known_observation_ids=json.loads(row["known_observation_ids"] or "[]"),
            version=row["version"] or 0,
            created_at=row["created_at"] or self._now(),
            metadata=json.loads(row["metadata"] or "{}"),
        )

    def save_bundle(self, bundle: ContextBundle) -> None:
        with self._lock:
            self._ensure_run(bundle.run_id)
            self._connection.execute(
                """
                insert or replace into context_bundles(
                    bundle_id, run_id, task_id, phase_id, model, provider, messages,
                    included_item_ids, excluded_item_ids, budget, compression_snapshot_id,
                    retrieval_query, render_policy, created_at, bundle_digest, metadata
                )
                values(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    bundle.bundle_id,
                    bundle.run_id,
                    bundle.task_id,
                    bundle.phase_id,
                    bundle.model,
                    bundle.provider,
                    json.dumps(bundle.messages, ensure_ascii=False, default=str),
                    json.dumps(bundle.included_item_ids, ensure_ascii=False),
                    json.dumps(bundle.excluded_item_ids, ensure_ascii=False),
                    json.dumps(bundle.budget.__dict__, ensure_ascii=False, default=str),
                    bundle.compression_snapshot_id,
                    bundle.retrieval_query,
                    json.dumps(bundle.render_policy.__dict__, ensure_ascii=False, default=str),
                    bundle.created_at,
                    bundle.bundle_digest,
                    json.dumps(bundle.metadata, ensure_ascii=False, default=str),
                ),
            )
            self._append_event(
                bundle.run_id,
                event_type="context.bundle_built",
                item_id=bundle.bundle_id,
                payload={
                    "included": len(bundle.included_item_ids),
                    "excluded": len(bundle.excluded_item_ids),
                    "budget": bundle.budget.__dict__,
                    "bundle_digest": bundle.bundle_digest,
                },
            )
            self._connection.commit()

    def latest_bundle(self, run_id: str) -> ContextBundle | None:
        row = self._connection.execute(
            """
            select * from context_bundles
            where run_id = ?
            order by created_at desc, bundle_id desc
            limit 1
            """,
            (run_id,),
        ).fetchone()
        if row is None:
            return None
        return ContextBundle.from_dict(
            {
                "bundle_id": row["bundle_id"],
                "run_id": row["run_id"],
                "task_id": row["task_id"],
                "phase_id": row["phase_id"],
                "model": row["model"],
                "provider": row["provider"],
                "messages": json.loads(row["messages"] or "[]"),
                "included_item_ids": json.loads(row["included_item_ids"] or "[]"),
                "excluded_item_ids": json.loads(row["excluded_item_ids"] or "[]"),
                "budget": json.loads(row["budget"] or "{}"),
                "compression_snapshot_id": row["compression_snapshot_id"],
                "retrieval_query": row["retrieval_query"],
                "render_policy": json.loads(row["render_policy"] or "{}"),
                "created_at": row["created_at"],
                "bundle_digest": row["bundle_digest"],
                "metadata": json.loads(row["metadata"] or "{}"),
            }
        )

    def save_summary(
        self,
        *,
        run_id: str,
        summary_id: str,
        payload: dict[str, Any],
        source_item_ids: list[str],
    ) -> None:
        self._connection.execute(
            """
            insert or replace into context_summaries(
                summary_id, run_id, payload, source_item_ids, created_at
            )
            values(?, ?, ?, ?, ?)
            """,
            (
                summary_id,
                run_id,
                json.dumps(payload, ensure_ascii=False, default=str),
                json.dumps(source_item_ids, ensure_ascii=False),
                self._now(),
            ),
        )
        self._connection.commit()

    def save_recovery_checkpoint(
        self,
        *,
        run_id: str,
        checkpoint_id: str,
        payload: dict[str, Any],
    ) -> None:
        self._connection.execute(
            """
            insert or replace into context_recovery_checkpoints(
                checkpoint_id, run_id, payload, created_at
            )
            values(?, ?, ?, ?)
            """,
            (
                checkpoint_id,
                run_id,
                json.dumps(payload, ensure_ascii=False, default=str),
                self._now(),
            ),
        )
        self._connection.commit()

    def latest_recovery_checkpoint(self, run_id: str) -> dict[str, Any] | None:
        row = self._connection.execute(
            """
            select payload from context_recovery_checkpoints
            where run_id = ?
            order by created_at desc, checkpoint_id desc
            limit 1
            """,
            (run_id,),
        ).fetchone()
        return json.loads(row["payload"]) if row else None

    def current_version(self, run_id: str) -> int:
        with self._lock:
            self._ensure_run(run_id)
            self._connection.commit()
            return self._run_version(run_id)

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
                current = self._run_version(run_id)
                if current != expected_version:
                    raise ContextVersionConflict(
                        f"Context version conflict for {run_id}: expected {expected_version}, got {current}"
                    )
                yield
                self._set_run_version(run_id, current + 1)
                self._connection.commit()
            except Exception:
                self._connection.rollback()
                raise

    def _init_schema(self) -> None:
        self._connection.executescript(
            """
            create table if not exists context_migrations(
                name text primary key,
                applied_at text not null
            );

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
                truncation_reason text,
                sensitivity text default 'workspace'
            );

            create table if not exists context_items(
                item_id text primary key,
                seq integer not null,
                run_id text not null,
                session_id text not null,
                task_id text not null,
                phase_id text not null,
                layer text not null,
                source_runtime text not null,
                item_type text not null,
                content text,
                content_digest text not null,
                created_at text not null,
                updated_at text not null,
                importance real not null,
                relevance_score real,
                authority text not null,
                freshness text not null,
                sensitivity text not null,
                token_count integer not null,
                refs text not null,
                metadata text not null,
                pinned integer not null,
                expires_at text
            );

            create table if not exists context_events(
                id integer primary key autoincrement,
                run_id text not null,
                seq integer not null,
                event_type text not null,
                item_id text,
                payload text not null,
                created_at text not null
            );

            create table if not exists context_bundles(
                bundle_id text primary key,
                run_id text not null,
                task_id text,
                phase_id text,
                model text,
                provider text,
                messages text not null,
                included_item_ids text not null,
                excluded_item_ids text not null,
                budget text not null,
                compression_snapshot_id text,
                retrieval_query text,
                render_policy text not null,
                created_at text not null,
                bundle_digest text not null,
                metadata text not null
            );

            create table if not exists context_references(
                id text primary key,
                observation_id text not null default '',
                type text not null default '',
                ref_type text not null default '',
                target text,
                path text,
                line_start integer,
                line_end integer,
                digest text,
                observed_at text,
                freshness text not null default 'current',
                source_item_id text not null default '',
                metadata text not null default '{}'
            );

            create table if not exists context_snapshots(
                snapshot_id text primary key,
                run_id text not null,
                session_id text,
                task_id text,
                goal text,
                summary text,
                retained_item_ids text not null,
                retained_messages text not null,
                known_observation_ids text not null,
                version integer not null,
                created_at text not null,
                metadata text not null default '{}'
            );

            create table if not exists context_summaries(
                summary_id text primary key,
                run_id text not null,
                payload text not null,
                source_item_ids text not null,
                created_at text not null
            );

            create table if not exists context_recovery_checkpoints(
                checkpoint_id text primary key,
                run_id text not null,
                payload text not null,
                created_at text not null
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
        self._mark_migration("0001_context_runtime")
        self._migrate_context_references()
        self._migrate_legacy_snapshots()
        self._ensure_observation_sensitivity_column()
        self._connection.commit()

    def _ensure_observation_sensitivity_column(self) -> None:
        columns = {
            row["name"]
            for row in self._connection.execute("pragma table_info(observations)").fetchall()
        }
        if "sensitivity" not in columns:
            self._connection.execute(
                "alter table observations add column sensitivity text default 'workspace'"
            )

    def _migrate_context_references(self) -> None:
        columns = {
            row["name"]
            for row in self._connection.execute("pragma table_info(context_references)").fetchall()
        }
        additions = {
            "ref_type": "text not null default ''",
            "target": "text",
            "observed_at": "text",
            "freshness": "text not null default 'current'",
            "source_item_id": "text not null default ''",
            "metadata": "text not null default '{}'",
        }
        for column, ddl in additions.items():
            if column not in columns:
                self._connection.execute(
                    f"alter table context_references add column {column} {ddl}"
                )
        self._connection.execute(
            """
            update context_references
            set ref_type = case when ref_type is null or ref_type = '' then type else ref_type end,
                target = case when target is null or target = '' then coalesce(path, observation_id, id) else target end,
                observed_at = case when observed_at is null or observed_at = '' then ? else observed_at end,
                freshness = case when freshness is null or freshness = '' then 'current' else freshness end,
                source_item_id = case when source_item_id is null or source_item_id = '' then observation_id else source_item_id end,
                metadata = case when metadata is null or metadata = '' then '{}' else metadata end
            """,
            (self._now(),),
        )

    def _migrate_legacy_snapshots(self) -> None:
        legacy_exists = self._connection.execute(
            "select name from sqlite_master where type='table' and name='snapshots'"
        ).fetchone()
        if legacy_exists is None:
            return
        rows = self._connection.execute(
            """
            select id, run_id, goal, summary, retained_messages,
                   known_observation_ids, version, created_at
            from snapshots
            where id not in (select snapshot_id from context_snapshots)
            """
        ).fetchall()
        for row in rows:
            self._connection.execute(
                """
                insert into context_snapshots(
                    snapshot_id, run_id, session_id, task_id, goal, summary,
                    retained_item_ids, retained_messages, known_observation_ids,
                    version, created_at, metadata
                )
                values(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    row["id"],
                    row["run_id"],
                    row["run_id"],
                    row["run_id"],
                    row["goal"],
                    row["summary"],
                    "[]",
                    row["retained_messages"],
                    row["known_observation_ids"],
                    row["version"],
                    row["created_at"],
                    json.dumps({"migrated_from": "snapshots"}),
                ),
            )

    def _mark_migration(self, name: str) -> None:
        self._connection.execute(
            """
            insert or ignore into context_migrations(name, applied_at)
            values(?, ?)
            """,
            (name, self._now()),
        )

    def _sanitize_item_for_storage(self, item: ContextItem) -> ContextItem:
        classified = self.classifier.classify(item.content)
        sensitivity = item.sensitivity
        if item.sensitivity == ContextSensitivity.SECRET or classified == ContextSensitivity.SECRET:
            sensitivity = ContextSensitivity.SECRET
        elif item.sensitivity == ContextSensitivity.SENSITIVE or classified == ContextSensitivity.SENSITIVE:
            sensitivity = ContextSensitivity.SENSITIVE
        content = item.content
        if sensitivity in {ContextSensitivity.SECRET, ContextSensitivity.SENSITIVE}:
            content = self.redactor.redact_value(item.content)
        return replace(
            item,
            content=content,
            sensitivity=sensitivity,
            content_digest=item.content_digest,
        )

    def _insert_context_item(self, item: ContextItem) -> None:
        seq = self._next_context_item_seq(item.run_id)
        self._connection.execute(
            """
            insert into context_items(
                item_id, seq, run_id, session_id, task_id, phase_id, layer,
                source_runtime, item_type, content, content_digest, created_at,
                updated_at, importance, relevance_score, authority, freshness,
                sensitivity, token_count, refs, metadata, pinned, expires_at
            )
            values(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            """,
            (
                item.item_id,
                seq,
                item.run_id,
                item.session_id,
                item.task_id,
                item.phase_id,
                item.layer.value,
                item.source_runtime.value,
                item.item_type.value,
                json.dumps(item.content, ensure_ascii=False, default=str),
                item.content_digest,
                item.created_at,
                item.updated_at,
                item.importance,
                item.relevance_score,
                item.authority.value,
                item.freshness.value,
                item.sensitivity.value,
                item.token_count,
                json.dumps([ref.to_dict() for ref in item.references], ensure_ascii=False, default=str),
                json.dumps(item.metadata, ensure_ascii=False, default=str),
                1 if item.pinned else 0,
                item.expires_at,
            ),
        )

    def _insert_reference(self, reference: ContextReference) -> None:
        self._connection.execute(
            """
            insert or replace into context_references(
                id, observation_id, type, ref_type, target, path, line_start, line_end,
                digest, observed_at, freshness, source_item_id, metadata
            )
            values(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            """,
            (
                reference.ref_id,
                reference.observation_id,
                reference.ref_type,
                reference.ref_type,
                reference.target,
                reference.path,
                reference.line_start,
                reference.line_end,
                reference.digest,
                reference.observed_at,
                reference.freshness.value,
                reference.source_item_id,
                json.dumps(reference.metadata, ensure_ascii=False, default=str),
            ),
        )

    def _append_event(
        self,
        run_id: str,
        *,
        event_type: str,
        item_id: str | None,
        payload: dict[str, Any],
    ) -> None:
        seq = self._next_event_seq(run_id)
        self._connection.execute(
            """
            insert into context_events(run_id, seq, event_type, item_id, payload, created_at)
            values(?, ?, ?, ?, ?, ?)
            """,
            (
                run_id,
                seq,
                event_type,
                item_id,
                json.dumps(payload, ensure_ascii=False, default=str),
                self._now(),
            ),
        )

    def _update_item_freshness(
        self,
        item_id: str,
        *,
        freshness: ContextFreshness,
        event_type: str,
        reason: str,
    ) -> None:
        row = self._connection.execute(
            "select run_id from context_items where item_id = ?",
            (item_id,),
        ).fetchone()
        if row is None:
            return
        self._connection.execute(
            """
            update context_items
            set freshness = ?, updated_at = ?
            where item_id = ?
            """,
            (freshness.value, self._now(), item_id),
        )
        self._append_event(
            row["run_id"],
            event_type=event_type,
            item_id=item_id,
            payload={"reason": reason},
        )
        self._connection.commit()

    def _item_from_row(self, row: sqlite3.Row) -> ContextItem:
        return ContextItem(
            item_id=row["item_id"],
            run_id=row["run_id"],
            session_id=row["session_id"],
            task_id=row["task_id"],
            phase_id=row["phase_id"],
            layer=row["layer"],
            source_runtime=row["source_runtime"],
            item_type=row["item_type"],
            content=json.loads(row["content"]) if row["content"] else None,
            content_digest=row["content_digest"],
            created_at=row["created_at"],
            updated_at=row["updated_at"],
            importance=float(row["importance"] or 0.5),
            relevance_score=row["relevance_score"],
            authority=row["authority"],
            freshness=row["freshness"],
            sensitivity=row["sensitivity"],
            token_count=int(row["token_count"] or 0),
            references=[
                ContextReference.from_dict(ref)
                for ref in json.loads(row["refs"] or "[]")
            ],
            metadata=json.loads(row["metadata"] or "{}"),
            pinned=bool(row["pinned"]),
            expires_at=row["expires_at"],
        )

    @staticmethod
    def _reference_from_row(row: sqlite3.Row) -> ContextReference:
        return ContextReference(
            ref_id=row["id"],
            ref_type=row["ref_type"] or row["type"],
            target=row["target"],
            path=row["path"],
            line_start=row["line_start"],
            line_end=row["line_end"],
            digest=row["digest"],
            observed_at=row["observed_at"] or datetime.now(UTC).isoformat(),
            freshness=row["freshness"] or ContextFreshness.CURRENT,
            source_item_id=row["source_item_id"] or "",
            metadata=json.loads(row["metadata"] or "{}"),
            observation_id=row["observation_id"] or "",
        )

    def _ensure_run(self, run_id: str) -> None:
        self._connection.execute(
            """
            insert or ignore into runs(run_id, version, created_at)
            values(?, 0, ?)
            """,
            (run_id, self._now()),
        )

    def _run_version(self, run_id: str) -> int:
        row = self._connection.execute(
            "select version from runs where run_id = ?",
            (run_id,),
        ).fetchone()
        return int(row["version"]) if row else 0

    def _set_run_version(self, run_id: str, version: int) -> None:
        self._connection.execute(
            "update runs set version = ? where run_id = ?",
            (version, run_id),
        )

    def _next_message_seq(self, run_id: str) -> int:
        row = self._connection.execute(
            "select coalesce(max(seq), -1) + 1 as next_seq from messages where run_id = ?",
            (run_id,),
        ).fetchone()
        return int(row["next_seq"])

    def _next_context_item_seq(self, run_id: str) -> int:
        row = self._connection.execute(
            "select coalesce(max(seq), -1) + 1 as next_seq from context_items where run_id = ?",
            (run_id,),
        ).fetchone()
        return int(row["next_seq"])

    def _next_event_seq(self, run_id: str) -> int:
        row = self._connection.execute(
            "select coalesce(max(seq), -1) + 1 as next_seq from context_events where run_id = ?",
            (run_id,),
        ).fetchone()
        return int(row["next_seq"])

    def _emit_trace(self, event_type: str, payload: dict[str, Any]) -> None:
        if self.trace is None or not hasattr(self.trace, "emit"):
            return
        self.trace.emit(
            event_type,
            runtime="context",
            summary=event_type,
            payload=payload,
            ids={"run_id": payload.get("run_id")},
        )

    @staticmethod
    def _now() -> str:
        return datetime.now(UTC).isoformat()


ContextStore = ObservationStore


def _value(value: Any) -> str | None:
    if value is None:
        return None
    return getattr(value, "value", str(value))
