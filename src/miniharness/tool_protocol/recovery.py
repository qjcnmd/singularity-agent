from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Any

from miniharness.tool_protocol.models import (
    ToolCallPhase,
    ToolProtocolRecoveryReport,
    ToolProtocolTurnResult,
    ToolProtocolTurnStatus,
)
from miniharness.tool_protocol.state import ToolProtocolStateStore


@dataclass(frozen=True)
class ToolProtocolRecoveryResult:
    status: ToolProtocolTurnStatus
    report: ToolProtocolRecoveryReport
    batch_id: str | None


class ToolProtocolRecoveryManager:
    def __init__(self, store: ToolProtocolStateStore | Path) -> None:
        self.store = store if isinstance(store, ToolProtocolStateStore) else ToolProtocolStateStore(store)

    def recover(self, *, run_id: str) -> ToolProtocolTurnResult:
        batches = self._batches_for_run(run_id)
        pending_call_ids: list[str] = []
        running_call_ids: list[str] = []
        pending_approval_call_ids: list[str] = []
        succeeded_but_not_appended_call_ids: list[str] = []
        missing_tool_messages: list[str] = []
        recovered_call_ids: list[str] = []
        warnings: list[str] = []

        for batch in batches:
            for record in self.store.records_for_batch(batch.batch_id):
                if record.phase in {ToolCallPhase.PROPOSED, ToolCallPhase.VALIDATED}:
                    pending_call_ids.append(record.envelope.tool_call_id)
                elif self._is_pending_approval(record):
                    pending_approval_call_ids.append(record.envelope.tool_call_id)
                elif record.phase == ToolCallPhase.RUNNING:
                    running_call_ids.append(record.envelope.tool_call_id)
                elif record.phase == ToolCallPhase.SUCCEEDED:
                    binding = self.store.result_binding(record.record_id)
                    if binding is not None and not binding.appended:
                        succeeded_but_not_appended_call_ids.append(record.envelope.tool_call_id)
                    if binding is not None and binding.appended and not record.context_message_id:
                        missing_tool_messages.append(record.envelope.assistant_message_id)
                    if binding is not None:
                        recovered_call_ids.append(record.envelope.tool_call_id)

        next_action = "request_model"
        status = ToolProtocolTurnStatus.PROCESSED
        if pending_approval_call_ids:
            next_action = "resume_pending_approval"
            status = ToolProtocolTurnStatus.PENDING_APPROVAL
        elif pending_call_ids:
            next_action = "execute_pending_tool"
            status = ToolProtocolTurnStatus.RECOVERED
        elif running_call_ids:
            next_action = "await_tool_result"
            status = ToolProtocolTurnStatus.RECOVERED
        elif succeeded_but_not_appended_call_ids:
            next_action = "append_tool_message"
            status = ToolProtocolTurnStatus.RECOVERED
        elif missing_tool_messages:
            next_action = "append_tool_message"
            status = ToolProtocolTurnStatus.RECOVERED

        report = ToolProtocolRecoveryReport(
            pending_call_ids=sorted(set(pending_call_ids)),
            running_call_ids=sorted(set(running_call_ids)),
            succeeded_but_not_appended_call_ids=sorted(set(succeeded_but_not_appended_call_ids)),
            assistant_message_ids_missing_tool_messages=sorted(set(missing_tool_messages)),
            recovered_call_ids=sorted(set(recovered_call_ids)),
            warnings=warnings
            + [
                f"pending approval: {call_id}"
                for call_id in sorted(set(pending_approval_call_ids))
            ],
            next_action=next_action,
        )
        return ToolProtocolTurnResult(
            status=status,
            batch_id=batches[0].batch_id if batches else None,
            pending_approval_count=len(set(pending_approval_call_ids)),
            appended_tool_message_count=len(recovered_call_ids),
            next_action=next_action,
            recovery_report=report.to_dict(),
        )

    def inspect(
        self,
        *,
        run_id: str | None = None,
        session_id: str | None = None,
        task_id: str | None = None,
    ) -> ToolProtocolRecoveryReport:
        _ = session_id, task_id
        return self._report_for_run(run_id or "")

    def _report_for_run(self, run_id: str) -> ToolProtocolRecoveryReport:
        return self._build_turn_result(run_id).recovery_report

    def _build_turn_result(self, run_id: str) -> ToolProtocolTurnResult:
        batches = self._batches_for_run(run_id)
        pending_call_ids: list[str] = []
        running_call_ids: list[str] = []
        pending_approval_call_ids: list[str] = []
        succeeded_but_not_appended_call_ids: list[str] = []
        missing_tool_messages: list[str] = []
        recovered_call_ids: list[str] = []
        warnings: list[str] = []

        for batch in batches:
            for record in self.store.records_for_batch(batch.batch_id):
                if record.phase in {ToolCallPhase.PROPOSED, ToolCallPhase.VALIDATED}:
                    pending_call_ids.append(record.envelope.tool_call_id)
                elif self._is_pending_approval(record):
                    pending_approval_call_ids.append(record.envelope.tool_call_id)
                elif record.phase == ToolCallPhase.RUNNING:
                    running_call_ids.append(record.envelope.tool_call_id)
                elif record.phase == ToolCallPhase.SUCCEEDED:
                    binding = self.store.result_binding(record.record_id)
                    if binding is not None and not binding.appended:
                        succeeded_but_not_appended_call_ids.append(record.envelope.tool_call_id)
                    if binding is not None and binding.appended and not record.context_message_id:
                        missing_tool_messages.append(record.envelope.assistant_message_id)
                    if binding is not None:
                        recovered_call_ids.append(record.envelope.tool_call_id)

        next_action = "request_model"
        status = ToolProtocolTurnStatus.PROCESSED
        if pending_approval_call_ids:
            next_action = "resume_pending_approval"
            status = ToolProtocolTurnStatus.PENDING_APPROVAL
        elif pending_call_ids:
            next_action = "execute_pending_tool"
            status = ToolProtocolTurnStatus.RECOVERED
        elif running_call_ids:
            next_action = "await_tool_result"
            status = ToolProtocolTurnStatus.RECOVERED
        elif succeeded_but_not_appended_call_ids:
            next_action = "append_tool_message"
            status = ToolProtocolTurnStatus.RECOVERED
        elif missing_tool_messages:
            next_action = "append_tool_message"
            status = ToolProtocolTurnStatus.RECOVERED

        report = ToolProtocolRecoveryReport(
            pending_call_ids=sorted(set(pending_call_ids)),
            running_call_ids=sorted(set(running_call_ids)),
            succeeded_but_not_appended_call_ids=sorted(set(succeeded_but_not_appended_call_ids)),
            assistant_message_ids_missing_tool_messages=sorted(set(missing_tool_messages)),
            recovered_call_ids=sorted(set(recovered_call_ids)),
            warnings=warnings
            + [
                f"pending approval: {call_id}"
                for call_id in sorted(set(pending_approval_call_ids))
            ],
            next_action=next_action,
        )
        return ToolProtocolTurnResult(
            status=status,
            batch_id=batches[0].batch_id if batches else None,
            pending_approval_count=len(set(pending_approval_call_ids)),
            appended_tool_message_count=len(recovered_call_ids),
            next_action=next_action,
            recovery_report=report.to_dict(),
        )

    def _batches_for_run(self, run_id: str) -> list[Any]:
        rows = self.store.connection.execute(
            "select * from tool_call_batches where run_id = ? order by created_at, batch_id",
            (run_id,),
        ).fetchall()
        return [self.store._batch_from_row(row) for row in rows]

    def _is_pending_approval(self, record: Any) -> bool:
        if record.phase == ToolCallPhase.WAITING_APPROVAL:
            return True
        binding = self.store.result_binding(record.record_id)
        result = binding.result if binding is not None else None
        return bool(
            record.phase == ToolCallPhase.FAILED
            and result is not None
            and result.error_code == "approval_required"
        )


ToolProtocolRecovery = ToolProtocolRecoveryManager
