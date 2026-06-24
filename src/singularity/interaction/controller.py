from __future__ import annotations

import warnings
from typing import Any, Protocol

from singularity.interaction.models import (
    ClarificationAnswer,
    ClarificationRequest,
    ControlCommand,
    DecisionPrompt,
    FinalReport,
    InteractionMode,
    OutcomeStatus,
    ProgressEvent,
    InteractionEvent,
    UserDecision,
)
from singularity.observability.models import TraceEvent, TraceEventType, TraceSeverity


class InteractionProvider(Protocol):
    def request_decision(self, prompt: DecisionPrompt) -> UserDecision:
        ...

    def request_clarification(self, request: ClarificationRequest) -> ClarificationAnswer:
        ...


class InteractionController:
    def __init__(
        self,
        *,
        mode: InteractionMode | str = InteractionMode.INTERACTIVE,
        trace: Any | None = None,
        provider: InteractionProvider | None = None,
        sinks: list[Any] | None = None,
        cancellation_manager: Any | None = None,
        fail_closed: bool = True,
    ) -> None:
        self.mode = InteractionMode(mode)
        self.trace = trace
        self.provider = provider
        self.sinks = list(sinks or [])
        self.cancellation_manager = cancellation_manager
        self.fail_closed = fail_closed
        self.events: list[InteractionEvent] = []
        self.decisions: list[UserDecision] = []
        self.clarifications: list[tuple[ClarificationRequest, ClarificationAnswer | None]] = []
        self.final_reports: list[FinalReport] = []

    def add_sink(self, sink: Any) -> None:
        self.sinks.append(sink)

    def publish(
        self,
        event: InteractionEvent | ProgressEvent,
        *,
        write_trace: bool = True,
    ) -> InteractionEvent:
        interaction_event = event.to_interaction_event() if isinstance(event, ProgressEvent) else event
        self.events.append(interaction_event)
        if write_trace:
            self._write_trace(interaction_event)
        for sink in list(self.sinks):
            try:
                self._deliver(sink, interaction_event)
            except Exception as exc:
                warnings.warn(
                    f"interaction sink failed: {type(exc).__name__}: {exc}",
                    RuntimeWarning,
                    stacklevel=2,
                )
        return interaction_event

    def consume_trace_event(self, trace_event: TraceEvent) -> InteractionEvent:
        interaction_event = interaction_event_from_trace_event(trace_event)
        return self.publish(interaction_event, write_trace=False)

    def request_decision(self, prompt: DecisionPrompt) -> UserDecision:
        self.publish(
            InteractionEvent(
                event_type="decision.prompted",
                summary=prompt.message,
                component="interaction",
                payload={"prompt": prompt.to_dict()},
                severity="warning" if prompt.risk_level in {"high", "critical"} else "info",
            )
        )
        if self.mode == InteractionMode.NON_INTERACTIVE:
            decision = self._non_interactive_decision(prompt)
        else:
            if self.provider is None:
                decision = UserDecision(
                    prompt_id=prompt.prompt_id,
                    decision="abort",
                    reason="interactive mode has no interaction provider",
                    metadata={"provider_missing": True, "fail_closed": True},
                )
            else:
                decision = self.provider.request_decision(prompt)
        self.decisions.append(decision)
        self.publish(
            InteractionEvent(
                event_type=TraceEventType.USER_DECISION_RECORDED.value,
                summary=f"User decision recorded: {decision.decision}.",
                component="interaction",
                payload={"decision": decision.to_dict(), "prompt": prompt.to_dict()},
                severity="warning"
                if decision.decision in {"reject", "abort", "revise"}
                else "info",
            )
        )
        return decision

    def request_clarification(
        self,
        request: ClarificationRequest,
        *,
        planner: Any | None = None,
    ) -> ClarificationAnswer:
        self.clarifications.append((request, None))
        self.publish(
            InteractionEvent(
                event_type=TraceEventType.CLARIFICATION_REQUESTED.value,
                summary=request.question,
                component="interaction",
                payload={"request": request.to_dict()},
                severity="warning" if request.required else "info",
            )
        )
        if self.mode == InteractionMode.NON_INTERACTIVE:
            answer = ClarificationAnswer(
                request_id=request.request_id,
                answer=str(request.metadata.get("default_answer") or ""),
                revised_goal=request.metadata.get("default_revised_goal"),
                answered_by="non-interactive-policy",
                metadata={"fail_closed": self.fail_closed},
            )
        elif self.provider is None:
            answer = ClarificationAnswer(
                request_id=request.request_id,
                answer="",
                answered_by="interaction-controller",
                metadata={"provider_missing": True, "fail_closed": True},
            )
        else:
            answer = self.provider.request_clarification(request)
        self.clarifications[-1] = (request, answer)
        if planner is not None and hasattr(planner, "record_clarification_answer"):
            planner.record_clarification_answer(request, answer)
        self.publish(
            InteractionEvent(
                event_type=TraceEventType.CLARIFICATION_ANSWERED.value,
                summary="Clarification answer recorded.",
                component="interaction",
                payload={
                    "request": request.to_dict(),
                    "answer": answer.to_dict(),
                    "goal_revision": answer.revised_goal,
                },
            )
        )
        return answer

    def handle_command(
        self,
        command: ControlCommand | str,
        *,
        message: str = "",
        cancellation_manager: Any | None = None,
    ) -> ControlCommand:
        resolved = ControlCommand(command)
        self.publish(
            InteractionEvent(
                event_type=TraceEventType.CONTROL_COMMAND_RECEIVED.value,
                summary=f"Control command received: {resolved.value}.",
                component="interaction",
                payload={"command": resolved.value, "message": message},
                severity="warning" if resolved in {ControlCommand.CANCEL, ControlCommand.ABORT} else "info",
            )
        )
        if resolved == ControlCommand.CANCEL:
            manager = cancellation_manager or self.cancellation_manager
            if manager is not None and hasattr(manager, "cancel"):
                manager.cancel(message=message or "cancel")
            self.publish(
                InteractionEvent(
                    event_type=TraceEventType.CANCELLATION_REQUESTED.value,
                    summary=message or "Cancellation requested by user.",
                    component="interaction",
                    payload={"reason": "user_interrupted"},
                    severity="warning",
                )
            )
        return resolved

    def build_final_report(
        self,
        *,
        planner_report: Any | None = None,
        kernel_report: Any | None = None,
        workspace_summary: dict[str, Any] | None = None,
        verification_summary: dict[str, Any] | None = None,
        review_findings: list[dict[str, Any]] | None = None,
        trace_summary: dict[str, Any] | None = None,
        error: BaseException | dict[str, Any] | None = None,
        cancelled: bool = False,
        cancellation_reason: str | None = None,
        blocked_reasons: list[str] | None = None,
        verification_required: bool = True,
    ) -> FinalReport:
        planner_payload = _to_dict(planner_report)
        kernel_payload = _to_dict(kernel_report)
        workspace_payload = dict(workspace_summary or kernel_payload.get("workspace_summary") or {})
        verification_payload = dict(
            verification_summary
            or planner_payload.get("verification_summary")
            or kernel_payload.get("verification_summary")
            or {}
        )
        trace_payload = dict(trace_summary or kernel_payload.get("trace_summary") or {})
        findings = list(review_findings or _review_findings(planner_payload))
        files_changed = _files_changed(planner_payload, workspace_payload)
        risks = list(planner_payload.get("risks") or [])
        blocked = list(blocked_reasons or [])
        blocked.extend(str(item) for item in planner_payload.get("blocked_reasons") or [])
        hard_blocked = sorted(
            {item for item in blocked if item and not _is_completion_gap(item)}
        )
        planner_status = str(planner_payload.get("status") or "").lower()
        verification_status = str(verification_payload.get("status") or "").lower() or None
        has_blocking_review = any(bool(item.get("blocking")) for item in findings if isinstance(item, dict))
        has_changes = bool(files_changed or planner_payload.get("agent_changes"))

        if cancelled:
            outcome = OutcomeStatus.CANCELLED
        elif hard_blocked or has_blocking_review:
            outcome = OutcomeStatus.BLOCKED
        elif error is not None:
            outcome = OutcomeStatus.FAILED
        elif (
            planner_status == "completed"
            and verification_status in {"ready", "ready_with_warnings"}
            and not has_blocking_review
        ):
            outcome = OutcomeStatus.SUCCESS
        elif verification_required and (has_changes or planner_status == "completed") and not verification_status:
            outcome = OutcomeStatus.UNVERIFIED
        elif has_changes:
            outcome = OutcomeStatus.PARTIAL_SUCCESS
        elif planner_status in {"failed", "error"}:
            outcome = OutcomeStatus.FAILED
        else:
            outcome = OutcomeStatus.UNVERIFIED

        report = FinalReport(
            outcome=outcome,
            summary=_outcome_summary(outcome, verification_status, error),
            completed_items=_completed_items(outcome, planner_payload),
            partial_items=_partial_items(outcome, planner_payload),
            failed_items=_failed_items(outcome, error),
            blocked_reasons=hard_blocked,
            cancelled_reason=cancellation_reason,
            verification_status=verification_status,
            review_findings=findings,
            files_changed=files_changed,
            risks=risks,
            next_steps=list(planner_payload.get("next_steps") or []),
            trace_summary=trace_payload,
            technical_summary={
                "planner": planner_payload,
                "kernel": kernel_payload,
                "workspace": workspace_payload,
            },
        )
        self.final_reports.append(report)
        self.publish(
            InteractionEvent(
                event_type=TraceEventType.FINAL_REPORT_COMPLETED.value,
                summary=f"Final report completed: {report.outcome.value}.",
                component="interaction",
                payload={"final_report": report.to_dict()},
                severity="warning"
                if report.outcome
                in {
                    OutcomeStatus.PARTIAL_SUCCESS,
                    OutcomeStatus.CANCELLED,
                    OutcomeStatus.BLOCKED,
                    OutcomeStatus.UNVERIFIED,
                }
                else "error"
                if report.outcome == OutcomeStatus.FAILED
                else "info",
            )
        )
        return report

    def _non_interactive_decision(self, prompt: DecisionPrompt) -> UserDecision:
        default = (prompt.default_decision or "").lower().strip()
        explicit_non_interactive_approve = bool(
            prompt.metadata.get("allow_non_interactive_approve")
        )
        if default and (default != "approve" or explicit_non_interactive_approve):
            return UserDecision(
                prompt_id=prompt.prompt_id,
                decision=default,
                reason="non-interactive explicit default",
                decided_by="non-interactive-policy",
                metadata={"default_used": True},
            )
        decision = "reject" if self.fail_closed else "abort"
        return UserDecision(
            prompt_id=prompt.prompt_id,
            decision=decision,
            reason="non-interactive mode requires explicit safe default",
            decided_by="non-interactive-policy",
            metadata={"fail_closed": True},
        )

    def _write_trace(self, event: InteractionEvent) -> None:
        if self.trace is None or not hasattr(self.trace, "emit"):
            return
        self.trace.emit(
            _trace_type_for_interaction_event(event.event_type),
            component="interaction",
            summary=event.summary,
            payload={
                **event.payload,
                "_interaction_origin_event_id": event.event_id,
                "interaction_event_type": event.event_type,
                "source_component": event.component,
            },
            ids={
                "run_id": event.run_id,
                "session_id": event.session_id,
                "task_id": event.task_id,
                "phase_id": event.phase_id,
                "action_id": event.action_id,
            },
            severity=_trace_severity(event.severity),
        )

    @staticmethod
    def _deliver(sink: Any, event: InteractionEvent) -> None:
        if callable(sink):
            sink(event)
            return
        if hasattr(sink, "handle"):
            sink.handle(event)
            return
        raise TypeError(f"Unsupported interaction sink: {type(sink).__name__}")


def interaction_event_from_trace_event(event: TraceEvent) -> InteractionEvent:
    event_type = event.event_type.value if hasattr(event.event_type, "value") else str(event.event_type)
    severity = event.severity.value if hasattr(event.severity, "value") else str(event.severity)
    return InteractionEvent(
        event_id=f"interaction_from_{event.event_id}",
        event_type=event_type,
        summary=event.summary,
        component=event.component,
        payload=dict(event.payload or {}),
        severity=severity,
        run_id=event.run_id,
        session_id=event.session_id,
        task_id=event.task_id,
        phase_id=event.phase_id,
        action_id=event.action_id,
        trace_event_id=event.event_id,
        timestamp=event.timestamp,
    )


def _trace_type_for_interaction_event(event_type: str) -> TraceEventType:
    try:
        return TraceEventType(event_type)
    except ValueError:
        return TraceEventType.CONTEXT_OBSERVATION_ADDED


def _trace_severity(value: str) -> TraceSeverity:
    try:
        return TraceSeverity(value)
    except ValueError:
        return TraceSeverity.INFO


def _to_dict(value: Any | None) -> dict[str, Any]:
    if value is None:
        return {}
    if hasattr(value, "to_dict"):
        payload = value.to_dict()
        return payload if isinstance(payload, dict) else {}
    if isinstance(value, dict):
        return value
    return {"value": str(value)}


def _files_changed(planner_payload: dict[str, Any], workspace_payload: dict[str, Any]) -> list[str]:
    changed: set[str] = set(str(item) for item in planner_payload.get("files_changed") or [])
    for change in planner_payload.get("agent_changes") or []:
        if isinstance(change, dict):
            for path in change.get("changed_files") or []:
                changed.add(str(path))
        elif change:
            changed.add(str(change))
    for path in workspace_payload.get("agent_changes") or []:
        changed.add(str(path))
    return sorted(changed)


def _review_findings(planner_payload: dict[str, Any]) -> list[dict[str, Any]]:
    review_summary = planner_payload.get("review_summary") or {}
    findings = review_summary.get("findings") if isinstance(review_summary, dict) else None
    return list(findings or [])


def _completed_items(outcome: OutcomeStatus, planner_payload: dict[str, Any]) -> list[str]:
    if outcome != OutcomeStatus.SUCCESS:
        return []
    files = planner_payload.get("files_changed") or []
    if files:
        return [f"Changed {len(files)} file(s)."]
    return ["Task completed."]


def _partial_items(outcome: OutcomeStatus, planner_payload: dict[str, Any]) -> list[str]:
    if outcome != OutcomeStatus.PARTIAL_SUCCESS:
        return []
    files = planner_payload.get("files_changed") or []
    return [f"Applied changes to {len(files)} file(s)."] if files else ["Work partially completed."]


def _failed_items(outcome: OutcomeStatus, error: BaseException | dict[str, Any] | None) -> list[str]:
    if outcome != OutcomeStatus.FAILED:
        return []
    if isinstance(error, BaseException):
        return [f"{type(error).__name__}: {error}"]
    if isinstance(error, dict):
        message = error.get("message") or error.get("type")
        return [str(message)] if message else ["Execution failed."]
    return ["Execution failed."]


def _outcome_summary(
    outcome: OutcomeStatus,
    verification_status: str | None,
    error: BaseException | dict[str, Any] | None,
) -> str:
    if outcome == OutcomeStatus.SUCCESS:
        return "Task completed with verification evidence."
    if outcome == OutcomeStatus.PARTIAL_SUCCESS:
        return "Task made changes but still has unresolved failures or risks."
    if outcome == OutcomeStatus.FAILED:
        if isinstance(error, BaseException):
            return f"Task failed: {type(error).__name__}: {error}"
        return "Task failed."
    if outcome == OutcomeStatus.CANCELLED:
        return "Task was cancelled before completion."
    if outcome == OutcomeStatus.BLOCKED:
        return "Task is blocked by policy, approval, clarification, or review findings."
    if not verification_status:
        return "Task result is unverified because required verification evidence is missing."
    return "Task result is unverified."


def _is_completion_gap(reason: str) -> bool:
    normalized = reason.strip().lower()
    return normalized in {
        "required_files_inspected",
        "required_changes_applied",
        "required_verifications_passed",
        "unresolved_failures_empty",
        "workspace_health_acceptable",
        "risks_acknowledged",
        "completion_criteria_unmet",
        "missing_required_evidence",
    }
