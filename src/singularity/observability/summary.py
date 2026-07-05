from __future__ import annotations

from collections.abc import Iterable
from typing import Any, TypedDict

from singularity.observability.models import (
    TraceArtifact,
    TraceEvent,
    TraceEventType,
    TraceSeverity,
    TraceSpan,
    TraceSummary,
)
from singularity.utils.serialization import coerce_int


class ModelUsageSummary(TypedDict):
    requests: int
    responses: int
    failures: int
    tool_calls_proposed: int
    input_tokens: int
    output_tokens: int
    total_tokens: int
    cached_input_tokens: int
    reasoning_tokens: int
    request_cache_hit_rates: dict[str, float]
    cache_miss_reasons: dict[str, list[str]]
    cache_attribution_sources: dict[str, str]
    cache_attribution_source_counts: dict[str, int]
    run_cache_hit_rate: float


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
                "component": event.component,
            }
            for event in selected_events
            if event.severity in {TraceSeverity.ERROR, TraceSeverity.CRITICAL}
        ][:10]
        key_artifacts = [artifact.artifact_id for artifact in selected_artifacts if not artifact.sensitive][:20]
        model_usage_summary = _model_usage_summary(selected_events)
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
            model_usage_summary=dict(model_usage_summary),
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
            "edit_events": len(
                [
                    event
                    for event in selected_events
                    if event.event_type
                    in {
                        TraceEventType.EDIT_PLAN_CREATED,
                        TraceEventType.EDIT_PATCH_VALIDATED,
                        TraceEventType.EDIT_APPLIED,
                        TraceEventType.EDIT_REPAIR_ATTEMPTED,
                        TraceEventType.EDIT_FAILED,
                    }
                ]
            ),
            "verification_checks": summary.verification_count,
            "policy_denials": summary.policy_denial_count,
            "approvals": summary.approval_count,
            "replans": summary.replan_count,
            "model_usage_summary": summary.model_usage_summary,
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
                TraceEventType.EDIT_PLAN_CREATED,
                TraceEventType.EDIT_PATCH_VALIDATED,
                TraceEventType.EDIT_APPLIED,
                TraceEventType.EDIT_REPAIR_ATTEMPTED,
                TraceEventType.EDIT_FAILED,
            }:
                lines.append(f"[trace] Edit: {event.summary}")
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


def _model_usage_summary(events: list[TraceEvent]) -> ModelUsageSummary:
    usage: ModelUsageSummary = {
        "requests": 0,
        "responses": 0,
        "failures": 0,
        "tool_calls_proposed": 0,
        "input_tokens": 0,
        "output_tokens": 0,
        "total_tokens": 0,
        "cached_input_tokens": 0,
        "reasoning_tokens": 0,
        "request_cache_hit_rates": {},
        "cache_miss_reasons": {},
        "cache_attribution_sources": {},
        "cache_attribution_source_counts": {
            "provider_native": 0,
            "component_inferred": 0,
            "unknown": 0,
        },
        "run_cache_hit_rate": 0.0,
    }
    for event in events:
        if event.event_type == TraceEventType.MODEL_REQUEST_CREATED:
            usage["requests"] += 1
        elif event.event_type == TraceEventType.MODEL_RESPONSE_RECEIVED:
            usage["responses"] += 1
            payload_usage = event.payload.get("usage") or {}
            if isinstance(payload_usage, dict):
                for key in (
                    "input_tokens",
                    "output_tokens",
                    "total_tokens",
                    "cached_input_tokens",
                    "reasoning_tokens",
                ):
                    usage[key] += _safe_int(payload_usage.get(key))
                input_tokens = _safe_int(payload_usage.get("input_tokens"))
                cached_tokens = _safe_int(payload_usage.get("cached_input_tokens"))
                request_id = str(event.payload.get("request_id") or event.event_id)
                usage["request_cache_hit_rates"][request_id] = _cache_rate(
                    cached_tokens,
                    input_tokens,
                )
                reasons = event.payload.get("cache_miss_reasons") or (event.payload.get("cache") or {}).get("cache_miss_reasons") or []
                if reasons:
                    usage["cache_miss_reasons"][request_id] = [str(reason) for reason in reasons]
                attribution = (event.payload.get("cache") or {}).get("cache_attribution") or {}
                attribution_source = str(attribution.get("source") or "unknown")
                if attribution_source not in usage["cache_attribution_source_counts"]:
                    attribution_source = "unknown"
                usage["cache_attribution_sources"][request_id] = attribution_source
                usage["cache_attribution_source_counts"][attribution_source] += 1
        elif event.event_type == TraceEventType.MODEL_REQUEST_FAILED:
            usage["failures"] += 1
        elif event.event_type == TraceEventType.MODEL_TOOL_CALL_PROPOSED:
            usage["tool_calls_proposed"] += 1
    usage["run_cache_hit_rate"] = _cache_rate(
        usage["cached_input_tokens"],
        usage["input_tokens"],
    )
    return usage


def _safe_int(value: Any) -> int:
    if isinstance(value, bool) or value is None:
        return 0
    return coerce_int(value, bool_default=0)


def _cache_rate(cached_tokens: int, input_tokens: int) -> float:
    if input_tokens <= 0:
        return 0.0
    return round(cached_tokens / input_tokens, 4)
