from __future__ import annotations

from collections.abc import Iterable

from singularity.observability.models import TraceEvent, TraceTimelineItem


NOISY_EVENT_TYPES = {"command.output_chunk"}


class TraceTimelineBuilder:
    def build(
        self,
        events: Iterable[TraceEvent],
        *,
        run_id: str | None = None,
        task_id: str | None = None,
        phase_id: str | None = None,
        action_id: str | None = None,
    ) -> list[TraceTimelineItem]:
        items: list[TraceTimelineItem] = []
        for event in events:
            if run_id is not None and event.run_id != run_id:
                continue
            if task_id is not None and event.task_id != task_id:
                continue
            if phase_id is not None and event.phase_id != phase_id:
                continue
            if action_id is not None and event.action_id != action_id:
                continue
            if event.event_type.value in NOISY_EVENT_TYPES:
                continue
            items.append(
                TraceTimelineItem(
                    timestamp=event.timestamp,
                    event_id=event.event_id,
                    event_type=event.event_type.value,
                    runtime=event.runtime,
                    summary=event.summary,
                    severity=event.severity.value,
                    related_ids=_related_ids(event),
                    artifact_refs=event.artifact_refs,
                )
            )
        return sorted(items, key=lambda item: (item.timestamp, item.event_id))


def _related_ids(event: TraceEvent) -> list[str]:
    values = [
        event.policy_decision_id,
        event.approval_grant_id,
        event.sandbox_id,
        event.command_id,
        event.transaction_id,
        event.verification_id,
        event.span_id,
        event.parent_event_id,
    ]
    return [value for value in values if value]
