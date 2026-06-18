from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from miniharness.context.models import (
    ContextItemType,
    RecoveredContext,
)
from miniharness.context.store import ObservationStore


class RecoveryManager:
    def __init__(self, db_path: Path, *, trace_path: Path | None = None) -> None:
        self.store = ObservationStore(db_path)
        self.trace_path = trace_path

    def recover(self, run_id: str) -> RecoveredContext:
        messages = self.store.load_messages(run_id)
        context_items = self.store.query_items(run_id=run_id)
        completed_tool_call_ids = {
            message["tool_call_id"]
            for message in messages
            if message.get("role") == "tool" and message.get("tool_call_id")
        }
        pending_tool_calls = self._pending_tool_calls(messages, completed_tool_call_ids)
        pending_policy_approval = self._pending_policy_approval(context_items)
        open_mutations = self._open_mutations(context_items)
        active_processes = self._active_process_sessions(context_items)
        last_verification_status = self._last_verification_status(context_items)
        trace_last_event = self._last_trace_event()
        latest_bundle = self.store.latest_bundle(run_id)
        checkpoint = self.store.latest_recovery_checkpoint(run_id)
        warnings = self._warnings(
            pending_tool_calls=pending_tool_calls,
            pending_policy_approval=pending_policy_approval,
            open_mutations=open_mutations,
            active_processes=active_processes,
        )
        return RecoveredContext(
            run_id=run_id,
            messages=messages,
            context_items=context_items,
            last_bundle=latest_bundle,
            planner_state=self._latest_payload(context_items, ContextItemType.PLANNER_STATE),
            pending_tool_calls=pending_tool_calls,
            completed_tool_call_ids=completed_tool_call_ids,
            pending_policy_approval=pending_policy_approval,
            active_process_sessions=active_processes,
            open_mutation_transactions=open_mutations,
            last_verification_status=last_verification_status,
            last_safe_checkpoint=checkpoint,
            recommended_next_action=self._recommended_action(
                messages,
                pending_tool_calls=pending_tool_calls,
                pending_policy_approval=pending_policy_approval,
                open_mutations=open_mutations,
                active_processes=active_processes,
                last_verification_status=last_verification_status,
                trace_last_event=trace_last_event,
            ),
            recovery_warnings=warnings,
            trace_last_event=trace_last_event,
        )

    def _last_trace_event(self) -> str | None:
        if self.trace_path is None or not self.trace_path.exists():
            return None
        last_event: str | None = None
        for line in self.trace_path.read_text(encoding="utf-8").splitlines():
            if not line.strip():
                continue
            try:
                payload = json.loads(line)
            except json.JSONDecodeError:
                continue
            event = payload.get("event") or payload.get("event_type")
            if isinstance(event, str):
                last_event = _normalize_trace_event(event)
        return last_event

    @staticmethod
    def _pending_tool_calls(
        messages: list[dict[str, Any]],
        completed_tool_call_ids: set[str],
    ) -> list[dict[str, Any]]:
        pending: list[dict[str, Any]] = []
        for message in messages:
            if message.get("role") != "assistant":
                continue
            for tool_call in message.get("tool_calls") or []:
                call_id = tool_call.get("id")
                if call_id and call_id not in completed_tool_call_ids:
                    pending.append(tool_call)
        return pending

    @staticmethod
    def _pending_policy_approval(items: list[Any]) -> dict[str, Any] | None:
        for item in reversed(items):
            if item.item_type != ContextItemType.POLICY_OBSERVATION:
                continue
            content = item.content if isinstance(item.content, dict) else {}
            if content.get("outcome") in {"require_review", "ask_user", "escalate"} and not content.get("approval_grant_id"):
                return content
        return None

    @staticmethod
    def _open_mutations(items: list[Any]) -> list[str]:
        open_ids: list[str] = []
        for item in items:
            if item.item_type != ContextItemType.MUTATION_EVIDENCE:
                continue
            content = item.content if isinstance(item.content, dict) else {}
            if content.get("status") in {"open", "pending", "started"} and content.get("transaction_id"):
                open_ids.append(str(content["transaction_id"]))
        return open_ids

    @staticmethod
    def _active_process_sessions(items: list[Any]) -> list[str]:
        sessions: list[str] = []
        for item in items:
            if item.item_type != ContextItemType.COMMAND_OBSERVATION:
                continue
            content = item.content if isinstance(item.content, dict) else {}
            if content.get("status") in {"running", "started"} and content.get("command_id"):
                sessions.append(str(content["command_id"]))
        return sessions

    @staticmethod
    def _last_verification_status(items: list[Any]) -> str | None:
        for item in reversed(items):
            if item.item_type == ContextItemType.VERIFICATION_EVIDENCE and isinstance(item.content, dict):
                return str(item.content.get("status") or "unknown")
        return None

    @staticmethod
    def _latest_payload(items: list[Any], item_type: ContextItemType) -> dict[str, Any] | None:
        for item in reversed(items):
            if item.item_type == item_type and isinstance(item.content, dict):
                return item.content
        return None

    @staticmethod
    def _warnings(
        *,
        pending_tool_calls: list[dict[str, Any]],
        pending_policy_approval: dict[str, Any] | None,
        open_mutations: list[str],
        active_processes: list[str],
    ) -> list[str]:
        warnings: list[str] = []
        if pending_tool_calls:
            warnings.append("pending_tool_calls")
        if pending_policy_approval is not None:
            warnings.append("pending_policy_approval")
        if open_mutations:
            warnings.append("open_mutation_transactions")
        if active_processes:
            warnings.append("active_process_sessions")
        return warnings

    @staticmethod
    def _recommended_action(
        messages: list[dict[str, Any]],
        *,
        pending_tool_calls: list[dict[str, Any]],
        pending_policy_approval: dict[str, Any] | None,
        open_mutations: list[str],
        active_processes: list[str],
        last_verification_status: str | None,
        trace_last_event: str | None,
    ) -> str:
        if pending_policy_approval is not None:
            return "ask_user_for_pending_approval"
        if open_mutations:
            return "rollback_incomplete_mutation"
        if active_processes:
            return "resume_process_observation"
        if pending_tool_calls:
            return "execute_pending_tool"
        if last_verification_status in {"failed", "blocked", "timeout"}:
            return "run_verification"
        if trace_last_event == "model_request":
            return "request_model"
        if messages and messages[-1].get("role") == "tool":
            return "request_model"
        return "request_model"


def _normalize_trace_event(event: str) -> str:
    mapping = {
        "model.request.created": "model_request",
        "model.response.received": "model_response",
        "tool.dispatch.completed": "tool_result",
        "context.rendered_for_model": "model_request",
    }
    return mapping.get(event, event)
