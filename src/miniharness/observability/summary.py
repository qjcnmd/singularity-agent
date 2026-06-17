from __future__ import annotations

from collections.abc import Iterable
from typing import Any

from miniharness.observability.models import (
    TraceArtifact,
    TraceEvent,
    TraceEventType,
    TraceSeverity,
    TraceSpan,
    TraceSummary,
)


class TraceSummaryBuilder:
    def summarize(
        self,
        *,
        events: Iterable[TraceEvent],
        spans: Iterable[TraceSpan],
        artifacts: Iterable[TraceArtifact],
        run_id: str | None = None,
        task_id: str | None = None,
    ) -> TraceSummary:
        selected_events = [
            event
            for event in events
            if (run_id is None or event.run_id == run_id)
            and (task_id is None or event.task_id == task_id)
        ]
        selected_spans = [
            span
            for span in spans
            if (run_id is None or span.run_id == run_id)
            and (task_id is None or span.task_id == task_id)
        ]
        selected_artifacts = [
            artifact
            for artifact in artifacts
            if (run_id is None or artifact.run_id == run_id)
            and (task_id is None or artifact.task_id == task_id)
        ]
        event_types = [event.event_type for event in selected_events]
        critical = [
            {
                "event_id": event.event_id,
                "event_type": event.event_type.value,
                "summary": event.summary,
                "runtime": event.runtime,
            }
            for event in selected_events
            if event.severity in {TraceSeverity.ERROR, TraceSeverity.CRITICAL}
        ][:10]
        key_artifacts = [artifact.artifact_id for artifact in selected_artifacts if not artifact.sensitive][:20]
        return TraceSummary(
            run_id=run_id or _first([event.run_id for event in selected_events]),
            session_id=_first([event.session_id for event in selected_events]),
            task_id=task_id or _first([event.task_id for event in selected_events]),
            total_events=len(selected_events),
            total_spans=len(selected_spans),
            total_artifacts=len(selected_artifacts),
            action_count=sum(
                1
                for event_type in event_types
                if event_type
                in {
                    TraceEventType.ACTION_PROPOSED,
                    TraceEventType.ACTION_STARTED,
                    TraceEventType.ACTION_COMPLETED,
                    TraceEventType.ACTION_FAILED,
                }
            ),
            failed_action_count=event_types.count(TraceEventType.ACTION_FAILED),
            command_count=sum(
                1
                for event_type in event_types
                if event_type
                in {
                    TraceEventType.COMMAND_COMPLETED,
                    TraceEventType.COMMAND_FAILED,
                    TraceEventType.COMMAND_TIMEOUT,
                    TraceEventType.COMMAND_KILLED,
                }
            ),
            sandboxed_command_count=len(
                {
                    event.command_id
                    for event in selected_events
                    if event.command_id and event.sandbox_id
                }
            ),
            mutation_count=sum(
                1
                for event_type in event_types
                if event_type
                in {
                    TraceEventType.MUTATION_APPLIED,
                    TraceEventType.MUTATION_FAILED,
                    TraceEventType.MUTATION_TRANSACTION_STARTED,
                }
            ),
            verification_count=sum(
                1
                for event_type in event_types
                if event_type
                in {
                    TraceEventType.VERIFICATION_CHECK_COMPLETED,
                    TraceEventType.VERIFICATION_FAILED,
                    TraceEventType.VERIFICATION_PLAN_CREATED,
                }
            ),
            policy_denial_count=sum(
                1
                for event_type in event_types
                if event_type == TraceEventType.POLICY_BLOCKED
            ),
            approval_count=sum(
                1
                for event_type in event_types
                if event_type
                in {
                    TraceEventType.APPROVAL_REQUESTED,
                    TraceEventType.APPROVAL_GRANTED,
                    TraceEventType.APPROVAL_DENIED,
                }
            ),
            replan_count=event_types.count(TraceEventType.PLANNER_REPLAN_TRIGGERED),
            error_count=sum(
                1
                for event in selected_events
                if event.severity in {TraceSeverity.ERROR, TraceSeverity.CRITICAL}
            ),
            critical_events=critical,
            key_artifacts=key_artifacts,
        )

    def final_report_summary(
        self,
        *,
        events: Iterable[TraceEvent],
        spans: Iterable[TraceSpan],
        artifacts: Iterable[TraceArtifact],
        run_id: str | None = None,
        task_id: str | None = None,
    ) -> dict[str, Any]:
        summary = self.summarize(
            events=events,
            spans=spans,
            artifacts=artifacts,
            run_id=run_id,
            task_id=task_id,
        )
        selected_events = [
            event
            for event in events
            if (run_id is None or event.run_id == run_id)
            and (task_id is None or event.task_id == task_id)
        ]
        return {
            "total_actions": summary.action_count,
            "failed_actions": summary.failed_action_count,
            "tool_calls": len(
                [
                    event
                    for event in selected_events
                    if event.event_type
                    in {
                        TraceEventType.TOOL_DISPATCH_COMPLETED,
                        TraceEventType.TOOL_DISPATCH_FAILED,
                    }
                ]
            ),
            "commands_executed": summary.command_count,
            "sandboxed_commands": summary.sandboxed_command_count,
            "workspace_mutations": summary.mutation_count,
            "verification_checks": summary.verification_count,
            "policy_denials": summary.policy_denial_count,
            "approvals": summary.approval_count,
            "replans": summary.replan_count,
            "key_failures": [
                event.summary
                for event in selected_events
                if event.severity in {TraceSeverity.ERROR, TraceSeverity.CRITICAL}
            ][:10],
            "key_artifacts": summary.key_artifacts,
        }

    def context_summary(
        self,
        *,
        events: Iterable[TraceEvent],
        run_id: str | None = None,
        task_id: str | None = None,
        limit: int = 8,
    ) -> list[str]:
        selected_events = [
            event
            for event in events
            if (run_id is None or event.run_id == run_id)
            and (task_id is None or event.task_id == task_id)
        ]
        lines: list[str] = []
        for event in selected_events:
            if event.event_type == TraceEventType.COMMAND_FAILED:
                suffix = f", see artifact {event.artifact_refs[0]}" if event.artifact_refs else ""
                lines.append(f"[trace] Command failed: {event.summary}{suffix}")
            elif event.event_type == TraceEventType.POLICY_BLOCKED:
                lines.append(f"[trace] Policy blocked: {event.summary}")
            elif event.event_type == TraceEventType.SANDBOX_CAPABILITY_FAILED:
                lines.append(f"[trace] Sandbox unavailable: {event.summary}")
            elif event.event_type in {
                TraceEventType.VERIFICATION_FAILED,
                TraceEventType.VERIFICATION_CHECK_COMPLETED,
            } and event.severity in {TraceSeverity.WARNING, TraceSeverity.ERROR, TraceSeverity.CRITICAL}:
                lines.append(f"[trace] Verification issue: {event.summary}")
            elif event.event_type in {
                TraceEventType.MUTATION_TRANSACTION_STARTED,
                TraceEventType.MUTATION_APPLIED,
                TraceEventType.MUTATION_FAILED,
            }:
                lines.append(f"[trace] Mutation: {event.summary}")
            elif event.event_type == TraceEventType.PLANNER_REPLAN_TRIGGERED:
                lines.append(f"[trace] Replanned: {event.summary}")
        return lines[-limit:]


def _first(values: list[Any]) -> Any | None:
    for value in values:
        if value is not None:
            return value
    return None
