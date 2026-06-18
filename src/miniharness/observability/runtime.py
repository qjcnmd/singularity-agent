from __future__ import annotations

import sys
import time
from contextlib import contextmanager
from datetime import UTC, datetime
from pathlib import Path
from typing import Any, Iterator
from uuid import uuid4

from miniharness.observability.artifacts import TraceArtifactStore
from miniharness.observability.models import (
    TraceArtifact,
    TraceArtifactKind,
    TraceEvent,
    TraceEventType,
    TraceSeverity,
    TraceSpan,
    TraceStatus,
)
from miniharness.observability.redaction import TraceRedactor
from miniharness.observability.spans import SpanManager
from miniharness.observability.store import TraceStore
from miniharness.observability.summary import TraceSummaryBuilder


class TraceRuntime:
    def __init__(
        self,
        *,
        root: Path | str,
        run_id: str,
        session_id: str,
        store: TraceStore | None = None,
        artifacts: TraceArtifactStore | None = None,
        redactor: TraceRedactor | None = None,
        trace_dir: Path | str | None = None,
    ) -> None:
        self.root = Path(root)
        self.run_id = run_id
        self.session_id = session_id
        self.redactor = redactor or TraceRedactor()
        self.store = store or TraceStore(self.root, run_id=run_id, trace_dir=trace_dir)
        self.artifacts = artifacts or TraceArtifactStore(
            self.root,
            run_id=run_id,
            session_id=session_id,
            redactor=self.redactor,
            run_dir=self.store.run_dir,
        )
        self.spans = SpanManager(store=self.store, run_id=run_id, session_id=session_id)
        self.path = self.store.events_path
        self._started = time.perf_counter()

    @classmethod
    def create(
        cls,
        root: Path | str,
        *,
        run_id: str | None = None,
        session_id: str | None = None,
        trace_dir: Path | str | None = None,
    ) -> "TraceRuntime":
        resolved_run_id = run_id or _new_run_id()
        return cls(
            root=root,
            run_id=resolved_run_id,
            session_id=session_id or resolved_run_id,
            trace_dir=trace_dir,
        )

    def emit(
        self,
        event_type: TraceEventType | str,
        *,
        runtime: str,
        summary: str,
        payload: dict[str, Any] | None = None,
        ids: dict[str, Any] | None = None,
        severity: TraceSeverity | str = TraceSeverity.INFO,
        artifact_refs: list[str] | None = None,
        related_refs: dict[str, Any] | None = None,
    ) -> TraceEvent | dict[str, Any]:
        try:
            ids = ids or {}
            related_refs = related_refs or {}
            raw_payload = payload or {}
            redacted_payload = self.redactor.redact_payload(raw_payload)
            event = TraceEvent(
                event_id=f"event_{uuid4().hex[:12]}",
                event_type=event_type,
                run_id=str(ids.get("run_id") or self.run_id),
                session_id=str(ids.get("session_id") or self.session_id),
                task_id=ids.get("task_id"),
                phase_id=ids.get("phase_id"),
                action_id=ids.get("action_id"),
                parent_event_id=ids.get("parent_event_id"),
                timestamp=datetime.now(UTC),
                monotonic_ms=max(0, int((time.perf_counter() - self._started) * 1000)),
                runtime=runtime,
                severity=severity,
                summary=self.redactor.redact_text(summary),
                payload=redacted_payload,
                artifact_refs=artifact_refs or [],
                policy_decision_id=ids.get("policy_decision_id") or related_refs.get("policy_decision_id"),
                approval_grant_id=ids.get("approval_grant_id") or related_refs.get("approval_grant_id"),
                sandbox_id=ids.get("sandbox_id") or related_refs.get("sandbox_id"),
                command_id=ids.get("command_id") or related_refs.get("command_id"),
                transaction_id=ids.get("transaction_id") or related_refs.get("transaction_id"),
                verification_id=ids.get("verification_id") or related_refs.get("verification_id"),
                span_id=ids.get("span_id") or related_refs.get("span_id"),
                redaction_applied=True,
                payload_hash=self.redactor.hash_payload(raw_payload),
            )
            self.store.append_event(event)
            return event
        except Exception as exc:
            warning = {
                "warning": "trace_write_failed",
                "type": type(exc).__name__,
                "message": str(exc),
            }
            print(f"[miniharness trace warning] {warning}", file=sys.stderr)
            return warning

    def record(self, event: str, data: dict[str, Any]) -> TraceEvent | dict[str, Any]:
        event_type, runtime, summary, severity, ids = self._legacy_event(event, data)
        payload = self._legacy_payload(event, data)
        return self.emit(
            event_type,
            runtime=runtime,
            summary=summary,
            payload=payload,
            ids=ids,
            severity=severity,
            artifact_refs=_legacy_artifacts(payload),
        )

    def start_span(
        self,
        name: str,
        *,
        runtime: str,
        ids: dict[str, Any] | None = None,
        attributes: dict[str, Any] | None = None,
        parent_span_id: str | None = None,
    ) -> TraceSpan:
        return self.spans.start_span(
            name,
            runtime=runtime,
            ids=ids,
            attributes=self.redactor.redact_payload(attributes or {}),
            parent_span_id=parent_span_id,
        )

    def end_span(
        self,
        span_id: str,
        *,
        status: TraceStatus | str,
        error: BaseException | None = None,
    ) -> TraceSpan:
        return self.spans.end_span(span_id, status=status, error=error)

    @contextmanager
    def span(
        self,
        name: str,
        *,
        runtime: str,
        ids: dict[str, Any] | None = None,
        attributes: dict[str, Any] | None = None,
        parent_span_id: str | None = None,
    ) -> Iterator[TraceSpan]:
        with self.spans.span(
            name,
            runtime=runtime,
            ids=ids,
            attributes=self.redactor.redact_payload(attributes or {}),
            parent_span_id=parent_span_id,
        ) as span:
            yield span

    def write_artifact(
        self,
        *,
        kind: TraceArtifactKind | str,
        text: str | None = None,
        data: bytes | None = None,
        path: Path | str | None = None,
        task_id: str | None = None,
        summary: str = "",
        metadata: dict[str, Any] | None = None,
        sensitive: bool = False,
        content_type: str | None = None,
    ) -> TraceArtifact:
        artifact_kind = TraceArtifactKind(kind)
        artifact: TraceArtifact
        if text is not None:
            artifact = self.artifacts.write_text_artifact(
                kind=artifact_kind,
                text=text,
                task_id=task_id,
                summary=summary,
                metadata=metadata,
                sensitive=sensitive,
                content_type=content_type or "text/plain",
            )
        elif data is not None:
            artifact = self.artifacts.write_bytes_artifact(
                kind=artifact_kind,
                data=data,
                task_id=task_id,
                summary=summary,
                metadata=metadata,
                sensitive=sensitive,
                content_type=content_type or "application/octet-stream",
            )
        elif path is not None:
            artifact = self.artifacts.register_file_artifact(
                kind=artifact_kind,
                source_path=path,
                task_id=task_id,
                summary=summary,
                metadata=metadata,
                sensitive=sensitive,
            )
        else:
            raise ValueError("text, data, or path is required")
        self.store.append_artifact(artifact)
        return artifact

    def timeline(self, **filters: Any) -> list[Any]:
        return self.store.get_timeline(**filters)

    def summarize(self, **filters: Any) -> Any:
        return self.store.summarize(**filters)

    def context_summary(self, **filters: Any) -> list[str]:
        return TraceSummaryBuilder().context_summary(
            events=self.store.query_events(),
            run_id=filters.get("run_id"),
            task_id=filters.get("task_id"),
        )

    def final_report_summary(self, **filters: Any) -> dict[str, Any]:
        return TraceSummaryBuilder().final_report_summary(
            events=self.store.query_events(),
            spans=list(self.store.latest_spans().values()),
            artifacts=self.store.artifacts(),
            run_id=filters.get("run_id"),
            task_id=filters.get("task_id"),
        )

    @staticmethod
    def _legacy_event(
        event: str,
        data: dict[str, Any],
    ) -> tuple[TraceEventType, str, str, TraceSeverity, dict[str, Any]]:
        ids = {
            "session_id": data.get("session_id"),
            "task_id": data.get("task_id"),
            "phase_id": data.get("phase_id") or data.get("phase"),
            "action_id": data.get("action_id") or data.get("tool_call_id"),
            "policy_decision_id": data.get("decision_id") or data.get("policy_decision_id"),
            "approval_grant_id": data.get("approval_grant_id"),
            "sandbox_id": data.get("sandbox_id"),
            "command_id": data.get("command_id"),
            "transaction_id": data.get("transaction_id"),
            "verification_id": data.get("verification_check_id") or data.get("verification_id"),
        }
        if event == "planner":
            decision = data.get("decision")
            if decision == "start_task":
                return TraceEventType.TASK_STARTED, "planner", "Task started.", TraceSeverity.INFO, ids
            if decision == "replan":
                return TraceEventType.PLANNER_REPLAN_TRIGGERED, "planner", str(data.get("reason") or "Replan triggered."), TraceSeverity.WARNING, ids
            if decision == "assess_completion":
                return TraceEventType.PLANNER_COMPLETION_ASSESSED, "planner", "Completion assessed.", TraceSeverity.INFO, ids
            if decision == "finalize":
                return TraceEventType.FINAL_REPORT_COMPLETED, "final_report", "Final report completed.", TraceSeverity.INFO, ids
            if decision == "tool_result":
                failed = bool(data.get("error_code"))
                return (
                    TraceEventType.ACTION_FAILED if failed else TraceEventType.ACTION_COMPLETED,
                    "planner",
                    str(data.get("reason") or "Action result recorded."),
                    TraceSeverity.ERROR if failed else TraceSeverity.INFO,
                    ids,
                )
            return TraceEventType.ACTION_PROPOSED, "planner", str(data.get("reason") or "Planner event."), TraceSeverity.INFO, ids
        if event == "tool_call":
            failed = data.get("status") == "error" or bool(data.get("error_code"))
            return (
                TraceEventType.TOOL_DISPATCH_FAILED if failed else TraceEventType.TOOL_DISPATCH_COMPLETED,
                "tool",
                f"Tool {data.get('tool_name') or '<unknown>'} {'failed' if failed else 'completed'}.",
                TraceSeverity.ERROR if failed else TraceSeverity.INFO,
                ids,
            )
        if event == "model_request":
            return (
                TraceEventType.MODEL_REQUEST_CREATED,
                "model",
                f"Model request created for turn {data.get('turn')}.",
                TraceSeverity.INFO,
                ids,
            )
        if event == "model_response":
            return (
                TraceEventType.MODEL_RESPONSE_RECEIVED,
                "model",
                f"Model response received for turn {data.get('turn')}.",
                TraceSeverity.INFO,
                ids,
            )
        if event == "final_answer":
            return (
                TraceEventType.FINAL_REPORT_CREATED,
                "final_report",
                "Final answer created.",
                TraceSeverity.INFO,
                ids,
            )
        if event == "error":
            return (
                TraceEventType.TASK_FAILED,
                "system",
                str(data.get("message") or data.get("type") or "Runtime error."),
                TraceSeverity.ERROR,
                ids,
            )
        if event == "policy":
            outcome = data.get("outcome")
            blocked = outcome in {"deny", "require_review", "ask_user", "escalate", "sandbox_required"}
            return (
                TraceEventType.POLICY_BLOCKED if blocked else TraceEventType.POLICY_DECIDED,
                "policy",
                str(data.get("reason") or f"Policy {outcome}."),
                TraceSeverity.WARNING if blocked else TraceSeverity.INFO,
                ids,
            )
        if event == "command":
            semantic = data.get("semantic_status")
            error_code = data.get("error_code")
            if error_code in {"timeout", "idle_timeout"}:
                event_type = TraceEventType.COMMAND_TIMEOUT
            elif semantic not in {None, "succeeded"} or error_code:
                event_type = TraceEventType.COMMAND_FAILED
            else:
                event_type = TraceEventType.COMMAND_COMPLETED
            return (
                event_type,
                "command",
                _command_summary(data),
                TraceSeverity.ERROR if event_type != TraceEventType.COMMAND_COMPLETED else TraceSeverity.INFO,
                ids,
            )
        if event == "mutation":
            failed = bool(data.get("error_code")) or bool(data.get("rejected"))
            if data.get("operation_type") == "rollback":
                event_type = TraceEventType.MUTATION_ROLLBACK_COMPLETED
            elif failed:
                event_type = TraceEventType.MUTATION_FAILED
            elif data.get("applied"):
                event_type = TraceEventType.MUTATION_APPLIED
            else:
                event_type = TraceEventType.MUTATION_TRANSACTION_STARTED
            return (
                event_type,
                "mutation",
                str(data.get("path") or data.get("changeset_id") or "Mutation event."),
                TraceSeverity.ERROR if failed else TraceSeverity.INFO,
                ids,
            )
        if event == "verification":
            phase = data.get("phase")
            if phase == "plan":
                event_type = TraceEventType.VERIFICATION_PLAN_CREATED
            elif phase == "result":
                event_type = TraceEventType.VERIFICATION_CHECK_COMPLETED
            elif phase == "assessment":
                event_type = TraceEventType.PLANNER_COMPLETION_ASSESSED
            else:
                event_type = TraceEventType.VERIFICATION_EVIDENCE_RECORDED
            failed = data.get("status") in {"failed", "blocked", "timeout"} or data.get("failure_type")
            return (
                TraceEventType.VERIFICATION_FAILED if failed else event_type,
                "verification",
                str(data.get("check_kind") or data.get("verification_plan_id") or "Verification event."),
                TraceSeverity.WARNING if failed else TraceSeverity.INFO,
                ids,
            )
        return TraceEventType.CONTEXT_OBSERVATION_ADDED, event, str(event), TraceSeverity.INFO, ids

    def _legacy_payload(self, event: str, data: dict[str, Any]) -> dict[str, Any]:
        if event == "model_request":
            messages = data.get("messages") or []
            tools = data.get("tools") or []
            return {
                "turn": data.get("turn"),
                "message_count": len(messages),
                "tool_count": len(tools),
                "messages_hash": self.redactor.hash_payload({"messages": messages}),
                "tools_hash": self.redactor.hash_payload({"tools": tools}),
            }
        if event == "model_response":
            response = data.get("response") or {}
            choices = response.get("choices") if isinstance(response, dict) else []
            message = ((choices or [{}])[0].get("message") or {}) if choices else {}
            tool_calls = message.get("tool_calls") or []
            return {
                "turn": data.get("turn"),
                "choice_count": len(choices or []),
                "tool_call_count": len(tool_calls),
                "content_hash": self.redactor.hash_payload(
                    {"content": message.get("content")}
                ),
            }
        if event == "tool_result":
            result = data.get("result") if isinstance(data.get("result"), dict) else {}
            metadata = result.get("metadata") if isinstance(result, dict) else {}
            return {
                "turn": data.get("turn"),
                "tool_call_id": data.get("tool_call_id"),
                "name": data.get("name"),
                "ok": result.get("ok") if isinstance(result, dict) else None,
                "error_code": result.get("error_code") if isinstance(result, dict) else None,
                "truncated": result.get("truncated") if isinstance(result, dict) else None,
                "output_digest": metadata.get("output_digest") if isinstance(metadata, dict) else None,
            }
        if event == "final_answer":
            content = str(data.get("content") or "")
            return {
                "turn": data.get("turn"),
                "content_hash": self.redactor.hash_payload({"content": content}),
                "content_chars": len(content),
            }
        return data


ObservabilityRuntime = TraceRuntime


def _new_run_id() -> str:
    timestamp = datetime.now(UTC).strftime("%Y%m%dT%H%M%SZ")
    return f"{timestamp}-{uuid4().hex[:8]}"


def _legacy_artifacts(data: dict[str, Any]) -> list[str]:
    refs: list[str] = []
    for key in ("artifact_id", "artifact_path", "evidence_artifact"):
        if data.get(key):
            refs.append(str(data[key]))
    return refs


def _command_summary(data: dict[str, Any]) -> str:
    command = data.get("shell") or " ".join(str(part) for part in data.get("argv") or [])
    if data.get("semantic_status") == "succeeded":
        return f"Command completed: {command}".strip()
    if data.get("error_code"):
        return f"Command failed ({data.get('error_code')}): {command}".strip()
    return f"Command completed: {command}".strip()
