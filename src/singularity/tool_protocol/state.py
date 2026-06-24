from __future__ import annotations

import json
import sqlite3
from pathlib import Path
from threading import RLock
from typing import Any

from singularity.context.redaction import ContextRedactor
from singularity.tool_protocol.errors import ToolProtocolStateError
from singularity.tool_protocol.models import (
    ToolCallBatch,
    ToolCallEnvelope,
    ToolCallFailureKind,
    ToolCallPhase,
    ToolCallRecord,
    ToolProtocolEvent,
    ToolProtocolResultBinding,
    ToolProtocolResultEnvelope,
    _now,
)
from singularity.tools.models import ToolSideEffectKind


_STATE_REDACTOR = ContextRedactor()
_RAW_RESULT_KEYS = {"raw_result", "raw_args", "result"}


class ToolProtocolReplayDecision:
    def __init__(
        self,
        *,
        status: str,
        allowed: bool,
        previous_result: ToolProtocolResultEnvelope | None = None,
        message: str = "",
    ) -> None:
        self.status = status
        self.allowed = allowed
        self.previous_result = previous_result
        self.message = message


class ToolProtocolStateStore:
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

    def close(self) -> None:
        with self._lock:
            self._connection.close()

    def create_batch(self, batch: ToolCallBatch | dict[str, Any]) -> ToolCallBatch:
        batch_obj = batch if isinstance(batch, ToolCallBatch) else ToolCallBatch.from_dict(batch)
        with self._lock:
            self._connection.execute(
                """
                insert or replace into tool_call_batches(
                    batch_id, run_id, session_id, task_id, phase_id, model_request_id,
                    model_response_id, assistant_message, tool_calls, supports_parallel_execution,
                    max_tool_calls, created_at, batch_digest
                )
                values(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    batch_obj.batch_id,
                    batch_obj.run_id,
                    batch_obj.session_id,
                    batch_obj.task_id,
                    batch_obj.phase_id,
                    batch_obj.model_request_id,
                    batch_obj.model_response_id,
                    json.dumps(
                        _state_redact_value(batch_obj.assistant_message),
                        ensure_ascii=False,
                        default=str,
                    ),
                    json.dumps(
                        [_state_safe_envelope_dict(call) for call in batch_obj.tool_calls],
                        ensure_ascii=False,
                        default=str,
                    ),
                    1 if batch_obj.supports_parallel_execution else 0,
                    batch_obj.max_tool_calls,
                    batch_obj.created_at,
                    batch_obj.batch_digest,
                ),
            )
            for call in batch_obj.tool_calls:
                self.append_event(
                    batch_id=batch_obj.batch_id,
                    record_id=None,
                    tool_call_id=call.tool_call_id,
                    run_id=batch_obj.run_id,
                    event_type=ToolCallPhase.PROPOSED.value,
                    payload={"tool_call_id": call.tool_call_id, "tool_name": call.tool_name},
                )
            self._connection.commit()
        return batch_obj

    save_batch = create_batch

    def upsert_record(
        self,
        envelope: ToolCallEnvelope | dict[str, Any],
        *,
        phase: ToolCallPhase,
        batch_id: str | None = None,
        previous_phase: ToolCallPhase | None = None,
        policy_decision_id: str | None = None,
        approval_grant_id: str | None = None,
        execution_started_at: str | None = None,
        execution_finished_at: str | None = None,
        tool_result_digest: str | None = None,
        context_message_id: str | None = None,
        error_kind: ToolCallFailureKind | None = None,
        error_message: str | None = None,
        attempts: int = 1,
    ) -> ToolCallRecord:
        call = envelope if isinstance(envelope, ToolCallEnvelope) else ToolCallEnvelope.from_dict(envelope)
        batch_row = self._batch_row_for_call(call, batch_id=batch_id)
        if batch_row is None:
            raise ToolProtocolStateError(f"unknown_batch_for_call: {call.tool_call_id}")
        resolved_batch_id = str(batch_row["batch_id"])
        existing_row = self._record_row(call.run_id, call.tool_call_id)
        record = ToolCallRecord(
            record_id=str(existing_row["record_id"]) if existing_row else _record_id(call.tool_call_id),
            envelope=call,
            phase=phase,
            previous_phase=previous_phase
            or (
                ToolCallPhase(existing_row["phase"])
                if existing_row and existing_row["phase"]
                else None
            ),
            policy_decision_id=policy_decision_id or _row_value(existing_row, "policy_decision_id"),
            approval_grant_id=approval_grant_id or _row_value(existing_row, "approval_grant_id"),
            execution_started_at=execution_started_at or _row_value(existing_row, "execution_started_at"),
            execution_finished_at=execution_finished_at or _row_value(existing_row, "execution_finished_at"),
            tool_result_digest=tool_result_digest or _row_value(existing_row, "tool_result_digest"),
            context_message_id=context_message_id or _row_value(existing_row, "context_message_id"),
            error_kind=error_kind
            or (
                ToolCallFailureKind(existing_row["error_kind"])
                if existing_row and existing_row["error_kind"]
                else None
            ),
            error_message=error_message or _row_value(existing_row, "error_message"),
            attempts=max(int(attempts), int(existing_row["attempts"]) + 1 if existing_row else 1),
            created_at=_row_value(existing_row, "created_at") or call.proposed_at,
            updated_at=call.proposed_at,
        )
        with self._lock:
            if existing_row is None:
                self._connection.execute(
                    """
                    insert into tool_call_records(
                        record_id, batch_id, run_id, session_id, task_id, phase_id, model_request_id,
                        model_response_id, assistant_message_id, tool_call_id, tool_name, argument_digest,
                        raw_arguments, parsed_arguments, normalized_arguments, phase, previous_phase,
                        policy_decision_id, approval_grant_id, execution_started_at, execution_finished_at,
                        tool_result_digest, context_message_id, error_kind, error_message, attempts,
                        envelope, created_at, updated_at
                    )
                    values(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                    """,
                    _record_values(record, resolved_batch_id),
                )
            else:
                self._connection.execute(
                    """
                    update tool_call_records
                    set batch_id = ?, run_id = ?, session_id = ?, task_id = ?, phase_id = ?,
                        model_request_id = ?, model_response_id = ?, assistant_message_id = ?,
                        tool_name = ?, argument_digest = ?, raw_arguments = ?, parsed_arguments = ?,
                        normalized_arguments = ?, phase = ?, previous_phase = ?, policy_decision_id = ?,
                        approval_grant_id = ?, execution_started_at = ?, execution_finished_at = ?,
                        tool_result_digest = ?, context_message_id = ?, error_kind = ?, error_message = ?,
                        attempts = ?, envelope = ?, updated_at = ?
                    where record_id = ?
                    """,
                    _record_update_values(record, resolved_batch_id),
                )
            self.append_event(
                batch_id=resolved_batch_id,
                record_id=record.record_id,
                run_id=call.run_id,
                event_type=phase.value,
                payload={"tool_call_id": call.tool_call_id, "tool_name": call.tool_name},
            )
            self._connection.commit()
        return record

    def transition(
        self,
        tool_call_id: str,
        phase: ToolCallPhase,
        *,
        policy_decision_id: str | None = None,
        approval_grant_id: str | None = None,
        error_kind: ToolCallFailureKind | None = None,
        error_message: str | None = None,
        tool_result_digest: str | None = None,
    ) -> ToolCallRecord:
        record = self._record_by_tool_call_id(tool_call_id)
        if record is None:
            batch_row = self._batch_row_for_tool_call_id(tool_call_id)
            if batch_row is None:
                raise ToolProtocolStateError(f"unknown_tool_call_id: {tool_call_id}")
            call = self._tool_call_from_batch(batch_row, tool_call_id)
            return self.upsert_record(
                call,
                batch_id=str(batch_row["batch_id"]),
                phase=phase,
                policy_decision_id=policy_decision_id,
                approval_grant_id=approval_grant_id,
                error_kind=error_kind,
                error_message=error_message,
                tool_result_digest=tool_result_digest,
            )
        record_row = self._record_row_by_id(record.record_id)
        if record_row is None:
            raise ToolProtocolStateError(f"unknown_record_id: {record.record_id}")
        return self.upsert_record(
            record.envelope,
            batch_id=str(record_row["batch_id"]),
            phase=phase,
            previous_phase=record.phase,
            policy_decision_id=policy_decision_id,
            approval_grant_id=approval_grant_id,
            error_kind=error_kind,
            error_message=error_message,
            tool_result_digest=tool_result_digest,
        )

    def bind_result(
        self,
        record_id: str | None = None,
        *,
        tool_call_id: str | None = None,
        result: ToolProtocolResultEnvelope,
        raw_result_ref: str | None = None,
    ) -> ToolProtocolResultBinding:
        if record_id is None:
            if tool_call_id is None:
                raise ToolProtocolStateError("missing_record_or_tool_call_id")
            record = self.record_by_tool_call_id(tool_call_id)
            record_id = record.record_id
        else:
            record = self.get_record(record_id)
        row = self._record_row_by_id(record_id)
        if row is None:
            raise ToolProtocolStateError(f"unknown_record_id: {record_id}")
        with self._lock:
            existing = self._binding_row(record_id)
            binding = ToolProtocolResultBinding(
                binding_id=str(existing["binding_id"]) if existing else _binding_id(record_id),
                record_id=record_id,
                tool_call_id=row["tool_call_id"],
                result_id=str(existing["result_id"]) if existing else _result_id(record_id),
                result=result,
                raw_result_ref=raw_result_ref or _row_value(existing, "raw_result_ref") or result.raw_result_ref,
                context_message_id=_row_value(existing, "context_message_id"),
                result_digest=result.content_digest,
                appended=bool(existing["appended"]) if existing else False,
                created_at=_row_value(existing, "created_at") or _now(),
                metadata=dict(result.metadata),
            )
            self._connection.execute(
                """
                insert or replace into tool_result_bindings(
                    binding_id, record_id, tool_call_id, result_id, result_payload,
                    raw_result_ref, context_message_id, result_digest, appended, created_at,
                    updated_at, metadata
                )
                values(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    binding.binding_id,
                    binding.record_id,
                    binding.tool_call_id,
                    binding.result_id,
                    json.dumps(
                        _state_safe_result_payload(result.to_dict()),
                        ensure_ascii=False,
                        default=str,
                    ),
                    binding.raw_result_ref,
                    binding.context_message_id,
                    binding.result_digest,
                    1 if binding.appended else 0,
                    binding.created_at,
                    _now(),
                    json.dumps(
                        _state_safe_event_payload(binding.metadata),
                        ensure_ascii=False,
                        default=str,
                    ),
                ),
            )
            result_phase = _phase_for_bound_result(result)
            self._connection.execute(
                """
                update tool_call_records
                set phase = ?, tool_result_digest = ?, execution_finished_at = coalesce(execution_finished_at, ?),
                    updated_at = ?
                where record_id = ?
                """,
                (
                    result_phase.value,
                    result.content_digest,
                    _now(),
                    _now(),
                    record_id,
                ),
            )
            self.append_event(
                batch_id=row["batch_id"],
                record_id=record_id,
                run_id=row["run_id"],
                event_type=result_phase.value,
                payload=_state_safe_result_payload(result.to_dict()),
            )
            self._connection.commit()
        return binding

    def mark_result_appended(
        self,
        record_id: str,
        *,
        observation_id: str | None = None,
        context_message_id: str | None = None,
    ) -> None:
        self.mark_result_appended_with_observation(
            record_id,
            observation_id=observation_id,
            context_message_id=context_message_id,
        )

    def mark_result_appended_with_observation(
        self,
        record_id: str,
        *,
        observation_id: str | None = None,
        context_message_id: str | None = None,
    ) -> None:
        row = self._record_row_by_id(record_id)
        if row is None:
            raise ToolProtocolStateError(f"unknown_record_id: {record_id}")
        with self._lock:
            self._connection.execute(
                """
                update tool_call_records
                set phase = ?, previous_phase = ?, context_message_id = coalesce(?, context_message_id),
                    updated_at = ?
                where record_id = ?
                """,
                (
                    (
                        row["phase"]
                        if row["phase"] == ToolCallPhase.WAITING_APPROVAL.value
                        else ToolCallPhase.RESULT_APPENDED.value
                    ),
                    (
                        row["previous_phase"]
                        if row["phase"] == ToolCallPhase.WAITING_APPROVAL.value
                        else row["phase"]
                    ),
                    context_message_id,
                    _now(),
                    record_id,
                ),
            )
            self._connection.execute(
                """
                update tool_result_bindings
                set appended = 1, context_message_id = coalesce(?, context_message_id), updated_at = ?
                where record_id = ?
                """,
                (context_message_id, _now(), record_id),
            )
            self.append_event(
                batch_id=row["batch_id"],
                record_id=record_id,
                run_id=row["run_id"],
                event_type=ToolCallPhase.RESULT_APPENDED.value,
                payload={"context_message_id": context_message_id, "observation_id": observation_id},
            )
            self._connection.commit()

    def append_event(
        self,
        *,
        batch_id: str,
        record_id: str | None,
        tool_call_id: str | None = None,
        run_id: str,
        event_type: str,
        payload: dict[str, Any],
    ) -> ToolProtocolEvent:
        event = ToolProtocolEvent(
            event_id="",
            run_id=run_id,
            batch_id=batch_id,
            tool_call_id=tool_call_id or self._tool_call_id_for_record(record_id),
            event_type=event_type,
            payload=_state_safe_event_payload(payload),
        )
        with self._lock:
            self._connection.execute(
                """
                insert into tool_protocol_events(
                    batch_id, record_id, tool_call_id, run_id, event_type, payload, created_at
                )
                values(?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    batch_id,
                    record_id,
                    event.tool_call_id,
                    run_id,
                    event_type,
                    json.dumps(event.payload, ensure_ascii=False, default=str),
                    event.created_at,
                ),
            )
            event_id = int(self._connection.execute("select last_insert_rowid() as id").fetchone()["id"])
            self._connection.commit()
        return ToolProtocolEvent(
            event_id=str(event_id),
            run_id=run_id,
            batch_id=batch_id,
            tool_call_id=tool_call_id or self._tool_call_id_for_record(record_id),
            event_type=event_type,
            payload=dict(event.payload),
            created_at=event.created_at,
        )

    def get_batch(self, batch_id: str) -> ToolCallBatch:
        row = self._connection.execute(
            "select * from tool_call_batches where batch_id = ?",
            (batch_id,),
        ).fetchone()
        if row is None:
            raise ToolProtocolStateError(f"unknown_batch_id: {batch_id}")
        return self._batch_from_row(row)

    def get_record(self, record_id: str) -> ToolCallRecord:
        row = self._record_row_by_id(record_id)
        if row is None:
            raise ToolProtocolStateError(f"unknown_record_id: {record_id}")
        return self._record_from_row(row)

    def record_by_tool_call_id(self, tool_call_id: str) -> ToolCallRecord:
        row = self._connection.execute(
            "select * from tool_call_records where tool_call_id = ? order by created_at desc limit 1",
            (tool_call_id,),
        ).fetchone()
        if row is None:
            raise ToolProtocolStateError(f"unknown_tool_call_id: {tool_call_id}")
        return self._record_from_row(row)

    def get_record_by_call_id(self, run_id: str, tool_call_id: str) -> ToolCallRecord:
        row = self._record_row(run_id, tool_call_id)
        if row is None:
            raise ToolProtocolStateError(f"unknown_tool_call_id: {tool_call_id}")
        return self._record_from_row(row)

    def records_for_batch(self, batch_id: str) -> list[ToolCallRecord]:
        rows = self._connection.execute(
            "select * from tool_call_records where batch_id = ? order by created_at, record_id",
            (batch_id,),
        ).fetchall()
        return [self._record_from_row(row) for row in rows]

    def pending_records(self, *, batch_id: str | None = None) -> list[ToolCallRecord]:
        return self._records_for_states(
            {
                ToolCallPhase.PROPOSED,
                ToolCallPhase.VALIDATED,
                ToolCallPhase.WAITING_APPROVAL,
                ToolCallPhase.APPROVED,
                ToolCallPhase.SCHEDULED,
                ToolCallPhase.RUNNING,
            },
            batch_id=batch_id,
        )

    def pending_calls(
        self,
        *,
        run_id: str | None = None,
        session_id: str | None = None,
        task_id: str | None = None,
    ) -> list[ToolCallRecord]:
        return self._records_for_states(
            {
                ToolCallPhase.PROPOSED,
                ToolCallPhase.VALIDATED,
                ToolCallPhase.WAITING_APPROVAL,
                ToolCallPhase.APPROVED,
                ToolCallPhase.SCHEDULED,
                ToolCallPhase.RUNNING,
            },
            run_id=run_id,
            session_id=session_id,
            task_id=task_id,
        )

    def completed_records(self, *, batch_id: str | None = None) -> list[ToolCallRecord]:
        return self._records_for_states(
            {ToolCallPhase.SUCCEEDED, ToolCallPhase.RESULT_APPENDED, ToolCallPhase.RECOVERED},
            batch_id=batch_id,
        )

    def failed_records(self, *, batch_id: str | None = None) -> list[ToolCallRecord]:
        return self._records_for_states(
            {ToolCallPhase.REJECTED, ToolCallPhase.FAILED, ToolCallPhase.CANCELLED},
            batch_id=batch_id,
        )

    def events_for_batch(self, batch_id: str) -> list[ToolProtocolEvent]:
        rows = self._connection.execute(
            "select * from tool_protocol_events where batch_id = ? order by event_id",
            (batch_id,),
        ).fetchall()
        return [self._event_from_row(row) for row in rows]

    def events_for_run(self, run_id: str) -> list[ToolProtocolEvent]:
        rows = self._connection.execute(
            "select * from tool_protocol_events where run_id = ? order by event_id",
            (run_id,),
        ).fetchall()
        return [self._event_from_row(row) for row in rows]

    def result_binding(self, record_id: str) -> ToolProtocolResultBinding | None:
        row = self._binding_row(record_id)
        return self._binding_from_row(row) if row else None

    def result_binding_by_tool_call_id(self, tool_call_id: str) -> ToolProtocolResultBinding | None:
        row = self._connection.execute(
            """
            select b.*
            from tool_result_bindings b
            join tool_call_records r on r.record_id = b.record_id
            where r.tool_call_id = ?
            order by b.created_at desc
            limit 1
            """,
            (tool_call_id,),
        ).fetchone()
        return self._binding_from_row(row) if row else None

    def resolve_replay(
        self,
        envelope: ToolCallEnvelope | dict[str, Any],
        *,
        side_effects: ToolSideEffectKind | str | None = None,
        idempotent: bool = True,
    ) -> ToolProtocolResultEnvelope | None:
        decision = self.check_replay(
            envelope,
            side_effects=side_effects,
            idempotent=idempotent,
        )
        return decision.previous_result

    def check_replay(
        self,
        envelope: ToolCallEnvelope | dict[str, Any],
        *,
        registry: Any | None = None,
        side_effects: ToolSideEffectKind | str | None = None,
        idempotent: bool | None = None,
    ) -> ToolProtocolReplayDecision:
        call = envelope if isinstance(envelope, ToolCallEnvelope) else ToolCallEnvelope.from_dict(envelope)
        spec = registry.get(call.tool_name) if registry is not None and hasattr(registry, "get") else None
        resolved_side_effects = side_effects
        resolved_idempotent = idempotent
        if spec is not None:
            resolved_side_effects = resolved_side_effects or getattr(spec, "side_effects", None)
            if resolved_idempotent is None:
                idempotency = getattr(spec, "idempotency_policy", None)
                resolved_idempotent = bool(
                    getattr(idempotency, "idempotent", getattr(spec, "idempotent", True))
                )
        if resolved_idempotent is None:
            resolved_idempotent = True
        row = self._record_row(call.run_id, call.tool_call_id)
        if row is None:
            return ToolProtocolReplayDecision(
                status="miss",
                allowed=False,
                previous_result=None,
                message="no_previous_result",
            )
        binding = self._binding_row(row["record_id"])
        if binding is None:
            return ToolProtocolReplayDecision(
                status="miss",
                allowed=False,
                previous_result=None,
                message="no_previous_result",
            )
        if not resolved_idempotent or _is_side_effectful(resolved_side_effects):
            return ToolProtocolReplayDecision(
                status="side_effect_replay",
                allowed=False,
                previous_result=None,
                message="side_effect_replay",
            )
        if str(row["argument_digest"]) != call.argument_digest:
            return ToolProtocolReplayDecision(
                status=ToolCallFailureKind.conflicting_replay.value,
                allowed=False,
                previous_result=None,
                message=ToolCallFailureKind.conflicting_replay.value,
            )
        return ToolProtocolReplayDecision(
            status="read_only_replay",
            allowed=True,
            previous_result=self._binding_from_row(binding).result,
            message="read_only_replay",
        )

    def append_recovered(
        self,
        *,
        batch_id: str,
        record_id: str,
        run_id: str,
        reason: str,
    ) -> ToolProtocolEvent:
        return self.append_event(
            batch_id=batch_id,
            record_id=record_id,
            run_id=run_id,
            event_type=ToolCallPhase.RECOVERED.value,
            payload={"reason": reason},
        )

    def _init_schema(self) -> None:
        self._connection.executescript(
            """
            create table if not exists tool_call_batches(
                batch_id text primary key,
                run_id text not null,
                session_id text not null,
                task_id text not null,
                phase_id text not null,
                model_request_id text not null,
                model_response_id text not null,
                assistant_message text not null,
                tool_calls text not null,
                supports_parallel_execution integer not null,
                max_tool_calls integer not null,
                created_at text not null,
                batch_digest text not null
            );

            create table if not exists tool_call_records(
                record_id text primary key,
                batch_id text not null,
                run_id text not null,
                session_id text not null,
                task_id text not null,
                phase_id text not null,
                model_request_id text not null,
                model_response_id text not null,
                assistant_message_id text not null,
                tool_call_id text not null,
                tool_name text not null,
                argument_digest text not null,
                raw_arguments text not null,
                parsed_arguments text not null,
                normalized_arguments text not null,
                phase text not null,
                previous_phase text,
                policy_decision_id text,
                approval_grant_id text,
                execution_started_at text,
                execution_finished_at text,
                tool_result_digest text,
                context_message_id text,
                error_kind text,
                error_message text,
                attempts integer not null,
                envelope text not null,
                created_at text not null,
                updated_at text not null
            );

            create table if not exists tool_protocol_events(
                event_id integer primary key autoincrement,
                batch_id text not null,
                record_id text,
                tool_call_id text,
                run_id text not null,
                event_type text not null,
                payload text not null,
                created_at text not null
            );

            create table if not exists tool_result_bindings(
                binding_id text primary key,
                record_id text not null,
                tool_call_id text not null,
                result_id text not null,
                result_payload text not null,
                raw_result_ref text,
                context_message_id text,
                result_digest text,
                appended integer not null,
                created_at text not null,
                updated_at text not null,
                metadata text not null
            );
        """
        )
        self._ensure_column("tool_protocol_events", "tool_call_id", "text")
        self._connection.commit()

    def _ensure_column(self, table: str, column: str, definition: str) -> None:
        columns = {
            str(row["name"])
            for row in self._connection.execute(f"pragma table_info({table})").fetchall()
        }
        if column not in columns:
            self._connection.execute(f"alter table {table} add column {column} {definition}")

    def _records_for_states(
        self,
        states: set[ToolCallPhase],
        *,
        batch_id: str | None = None,
        run_id: str | None = None,
        session_id: str | None = None,
        task_id: str | None = None,
    ) -> list[ToolCallRecord]:
        params: list[Any] = [state.value for state in states]
        clauses = ["phase in (" + ",".join("?" for _ in states) + ")"]
        if batch_id is not None:
            clauses.append("batch_id = ?")
            params.append(batch_id)
        if run_id is not None:
            clauses.append("run_id = ?")
            params.append(run_id)
        if session_id is not None:
            clauses.append("session_id = ?")
            params.append(session_id)
        if task_id is not None:
            clauses.append("task_id = ?")
            params.append(task_id)
        rows = self._connection.execute(
            "select * from tool_call_records where " + " and ".join(clauses) + " order by created_at, record_id",
            params,
        ).fetchall()
        return [self._record_from_row(row) for row in rows]

    def _record_row(self, run_id: str, tool_call_id: str) -> sqlite3.Row | None:
        return self._connection.execute(
            "select * from tool_call_records where run_id = ? and tool_call_id = ? order by created_at desc limit 1",
            (run_id, tool_call_id),
        ).fetchone()

    def _record_by_tool_call_id(self, tool_call_id: str) -> ToolCallRecord | None:
        row = self._connection.execute(
            "select * from tool_call_records where tool_call_id = ? order by created_at desc limit 1",
            (tool_call_id,),
        ).fetchone()
        return self._record_from_row(row) if row else None

    def _record_row_by_id(self, record_id: str) -> sqlite3.Row | None:
        return self._connection.execute(
            "select * from tool_call_records where record_id = ?",
            (record_id,),
        ).fetchone()

    def _binding_row(self, record_id: str) -> sqlite3.Row | None:
        return self._connection.execute(
            "select * from tool_result_bindings where record_id = ?",
            (record_id,),
        ).fetchone()

    def _tool_call_id_for_record(self, record_id: str | None) -> str | None:
        if record_id is None:
            return None
        row = self._record_row_by_id(record_id)
        return str(row["tool_call_id"]) if row else None

    def _batch_row_for_call(self, call: ToolCallEnvelope, *, batch_id: str | None = None) -> sqlite3.Row | None:
        if batch_id is not None:
            return self._connection.execute(
                "select * from tool_call_batches where batch_id = ?",
                (batch_id,),
            ).fetchone()
        return self._connection.execute(
            """
            select *
            from tool_call_batches
            where run_id = ? and model_request_id = ? and model_response_id = ? and phase_id = ?
            order by created_at desc
            limit 1
            """,
            (call.run_id, call.model_request_id, call.model_response_id, call.phase_id),
        ).fetchone()

    def _batch_row_for_tool_call_id(self, tool_call_id: str) -> sqlite3.Row | None:
        rows = self._connection.execute(
            "select * from tool_call_batches order by created_at desc, batch_id desc",
        ).fetchall()
        for row in rows:
            calls = json.loads(row["tool_calls"] or "[]")
            for call in calls:
                call_id = str(call.get("tool_call_id") or call.get("id") or "")
                if call_id == tool_call_id:
                    return row
        return None

    def _tool_call_from_batch(self, row: sqlite3.Row, tool_call_id: str) -> ToolCallEnvelope:
        for call in json.loads(row["tool_calls"] or "[]"):
            call_id = str(call.get("tool_call_id") or call.get("id") or "")
            if call_id == tool_call_id:
                return ToolCallEnvelope.from_dict(
                    {
                        **call,
                        "run_id": row["run_id"],
                        "session_id": row["session_id"],
                        "task_id": row["task_id"],
                        "phase_id": row["phase_id"],
                        "model_request_id": row["model_request_id"],
                        "model_response_id": row["model_response_id"],
                    }
                )
        raise ToolProtocolStateError(f"unknown_tool_call_id: {tool_call_id}")

    def batch_by_assistant_message_id(self, batch_id: str) -> ToolCallBatch | None:
        row = self._connection.execute(
            "select * from tool_call_batches where batch_id = ?",
            (batch_id,),
        ).fetchone()
        if row:
            return self._batch_from_row(row)
        rows = self._connection.execute(
            "select * from tool_call_batches order by created_at desc, batch_id desc",
        ).fetchall()
        for candidate in rows:
            assistant_message = json.loads(candidate["assistant_message"] or "{}")
            if str(assistant_message.get("id") or "") == batch_id:
                return self._batch_from_row(candidate)
            for call in json.loads(candidate["tool_calls"] or "[]"):
                if str(call.get("assistant_message_id") or "") == batch_id:
                    return self._batch_from_row(candidate)
        return None

    def _batch_from_row(self, row: sqlite3.Row) -> ToolCallBatch:
        return ToolCallBatch(
            batch_id=row["batch_id"],
            run_id=row["run_id"],
            session_id=row["session_id"],
            task_id=row["task_id"],
            phase_id=row["phase_id"],
            model_request_id=row["model_request_id"],
            model_response_id=row["model_response_id"],
            assistant_message=json.loads(row["assistant_message"] or "{}"),
            tool_calls=json.loads(row["tool_calls"] or "[]"),
            supports_parallel_execution=bool(row["supports_parallel_execution"]),
            max_tool_calls=int(row["max_tool_calls"] or 0),
            created_at=row["created_at"],
            batch_digest=row["batch_digest"],
        )

    def _record_from_row(self, row: sqlite3.Row) -> ToolCallRecord:
        return ToolCallRecord(
            record_id=row["record_id"],
            envelope=ToolCallEnvelope.from_dict(json.loads(row["envelope"])),
            phase=ToolCallPhase(row["phase"]),
            previous_phase=ToolCallPhase(row["previous_phase"]) if row["previous_phase"] else None,
            policy_decision_id=row["policy_decision_id"],
            approval_grant_id=row["approval_grant_id"],
            execution_started_at=row["execution_started_at"],
            execution_finished_at=row["execution_finished_at"],
            tool_result_digest=row["tool_result_digest"],
            context_message_id=row["context_message_id"],
            error_kind=ToolCallFailureKind(row["error_kind"]) if row["error_kind"] else None,
            error_message=row["error_message"],
            attempts=int(row["attempts"] or 1),
            created_at=row["created_at"],
            updated_at=row["updated_at"],
        )

    def _binding_from_row(self, row: sqlite3.Row) -> ToolProtocolResultBinding:
        return ToolProtocolResultBinding(
            binding_id=row["binding_id"],
            record_id=row["record_id"],
            tool_call_id=row["tool_call_id"],
            result_id=row["result_id"],
            result=ToolProtocolResultEnvelope.from_dict(json.loads(row["result_payload"])),
            raw_result_ref=row["raw_result_ref"],
            context_message_id=row["context_message_id"],
            result_digest=row["result_digest"],
            appended=bool(row["appended"]),
            created_at=row["created_at"],
            metadata=json.loads(row["metadata"] or "{}"),
        )

    def _event_from_row(self, row: sqlite3.Row) -> ToolProtocolEvent:
        return ToolProtocolEvent(
            event_id=str(row["event_id"]),
            run_id=row["run_id"],
            batch_id=row["batch_id"],
            tool_call_id=row["tool_call_id"] or self._tool_call_id_for_record(row["record_id"]),
            event_type=row["event_type"],
            payload=json.loads(row["payload"] or "{}"),
            created_at=row["created_at"],
        )


def _record_id(tool_call_id: str) -> str:
    return f"record_{tool_call_id}"


def _phase_for_bound_result(result: ToolProtocolResultEnvelope) -> ToolCallPhase:
    if result.ok:
        return ToolCallPhase.SUCCEEDED
    if result.error_code == ToolCallFailureKind.approval_required.value:
        return ToolCallPhase.WAITING_APPROVAL
    return ToolCallPhase.FAILED


def _binding_id(record_id: str) -> str:
    return f"binding_{record_id}"


def _result_id(record_id: str) -> str:
    return f"result_{record_id}"


def _record_values(record: ToolCallRecord, batch_id: str) -> tuple[Any, ...]:
    envelope = record.envelope
    safe_envelope = _state_safe_envelope_dict(envelope)
    return (
        record.record_id,
        batch_id,
        envelope.run_id,
        envelope.session_id,
        envelope.task_id,
        envelope.phase_id,
        envelope.model_request_id,
        envelope.model_response_id,
        envelope.assistant_message_id,
        envelope.tool_call_id,
        envelope.tool_name,
        envelope.argument_digest,
        safe_envelope["raw_arguments"],
        json.dumps(safe_envelope["parsed_arguments"], ensure_ascii=False, default=str),
        json.dumps(safe_envelope["normalized_arguments"], ensure_ascii=False, default=str),
        record.phase.value,
        record.previous_phase.value if record.previous_phase else None,
        record.policy_decision_id,
        record.approval_grant_id,
        record.execution_started_at,
        record.execution_finished_at,
        record.tool_result_digest,
        record.context_message_id,
        record.error_kind.value if record.error_kind else None,
        record.error_message,
        record.attempts,
        json.dumps(safe_envelope, ensure_ascii=False, default=str),
        record.created_at,
        record.updated_at,
    )


def _record_update_values(record: ToolCallRecord, batch_id: str) -> tuple[Any, ...]:
    envelope = record.envelope
    safe_envelope = _state_safe_envelope_dict(envelope)
    return (
        batch_id,
        envelope.run_id,
        envelope.session_id,
        envelope.task_id,
        envelope.phase_id,
        envelope.model_request_id,
        envelope.model_response_id,
        envelope.assistant_message_id,
        envelope.tool_name,
        envelope.argument_digest,
        safe_envelope["raw_arguments"],
        json.dumps(safe_envelope["parsed_arguments"], ensure_ascii=False, default=str),
        json.dumps(safe_envelope["normalized_arguments"], ensure_ascii=False, default=str),
        record.phase.value,
        record.previous_phase.value if record.previous_phase else None,
        record.policy_decision_id,
        record.approval_grant_id,
        record.execution_started_at,
        record.execution_finished_at,
        record.tool_result_digest,
        record.context_message_id,
        record.error_kind.value if record.error_kind else None,
        record.error_message,
        record.attempts,
        json.dumps(safe_envelope, ensure_ascii=False, default=str),
        record.updated_at,
        record.record_id,
    )


def _row_value(row: sqlite3.Row | None, key: str) -> Any:
    if row is None:
        return None
    return row[key]


def _is_side_effectful(side_effects: ToolSideEffectKind | str | None) -> bool:
    if side_effects is None:
        return False
    value = side_effects.value if isinstance(side_effects, ToolSideEffectKind) else str(side_effects)
    return value not in {
        ToolSideEffectKind.NONE.value,
        ToolSideEffectKind.READ_WORKSPACE.value,
    }


def _state_safe_envelope_dict(envelope: ToolCallEnvelope) -> dict[str, Any]:
    payload = envelope.to_dict()
    payload["raw_arguments"] = _state_redact_raw_arguments(envelope.raw_arguments)
    payload["parsed_arguments"] = _state_redact_value(envelope.parsed_arguments)
    payload["normalized_arguments"] = _state_redact_value(envelope.normalized_arguments)
    payload["metadata"] = _state_safe_event_payload(envelope.metadata)
    return _state_redact_value(payload)


def _state_safe_result_payload(payload: dict[str, Any]) -> dict[str, Any]:
    return _state_safe_event_payload(payload)


def _state_safe_event_payload(payload: dict[str, Any]) -> dict[str, Any]:
    safe = _drop_raw_result_keys(payload)
    redacted = _state_redact_value(safe)
    return redacted if isinstance(redacted, dict) else {}


def _state_redact_raw_arguments(raw_arguments: Any) -> str:
    if not isinstance(raw_arguments, str):
        raw_arguments = json.dumps(raw_arguments, ensure_ascii=False, default=str)
    try:
        parsed = json.loads(raw_arguments)
    except (TypeError, json.JSONDecodeError):
        return _STATE_REDACTOR.redact_text(str(raw_arguments))
    redacted = _state_redact_value(parsed)
    return json.dumps(redacted, ensure_ascii=False, default=str)


def _state_redact_value(value: Any) -> Any:
    return _STATE_REDACTOR.redact_value(value)


def _drop_raw_result_keys(value: Any) -> Any:
    if isinstance(value, dict):
        return {
            str(key): _drop_raw_result_keys(item)
            for key, item in value.items()
            if str(key) not in _RAW_RESULT_KEYS
        }
    if isinstance(value, list):
        return [_drop_raw_result_keys(item) for item in value]
    if isinstance(value, tuple):
        return [_drop_raw_result_keys(item) for item in value]
    return value
