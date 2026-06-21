from __future__ import annotations

import json
import os
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from miniharness.observability.exceptions import TraceStoreError
from miniharness.observability.models import (
    TraceArtifact,
    TraceEvent,
    TraceEventType,
    TraceSeverity,
    TraceSpan,
    TraceStatus,
    TraceSummary,
    TraceTimelineItem,
)
from miniharness.observability.summary import TraceSummaryBuilder
from miniharness.observability.timeline import TraceTimelineBuilder


class TraceStore:
    def __init__(
        self,
        root: Path | str,
        *,
        run_id: str,
        trace_dir: Path | str | None = None,
    ) -> None:
        self.root = Path(root)
        self.run_id = run_id
        self.trace_dir = Path(trace_dir).expanduser() if trace_dir is not None else None
        if self.trace_dir is not None:
            self.run_dir = self.trace_dir / run_id
        else:
            self.run_dir = self.root / "work" / "traces" / "runs" / run_id
        self.run_dir.mkdir(parents=True, exist_ok=True)
        self.events_path = self.run_dir / "events.jsonl"
        self.spans_path = self.run_dir / "spans.jsonl"
        self.artifacts_path = self.run_dir / "artifacts.jsonl"
        self.index_path = self.run_dir / "index.json"
        self._write_index()

    def append_event(self, event: TraceEvent) -> None:
        self._append_jsonl(self.events_path, event.to_dict())

    def append_span(self, span: TraceSpan) -> None:
        self._append_jsonl(self.spans_path, span.to_dict())

    def update_span_end(
        self,
        span_id: str,
        status: TraceStatus | str,
        *,
        error_type: str | None = None,
        error_message: str | None = None,
        ended_at: datetime | None = None,
        duration_ms: int | None = None,
    ) -> TraceSpan:
        latest = self.latest_spans().get(span_id)
        if latest is None:
            raise TraceStoreError(f"Unknown span: {span_id}")
        span = TraceSpan(
            span_id=latest.span_id,
            parent_span_id=latest.parent_span_id,
            run_id=latest.run_id,
            session_id=latest.session_id,
            task_id=latest.task_id,
            phase_id=latest.phase_id,
            action_id=latest.action_id,
            name=latest.name,
            runtime=latest.runtime,
            started_at=latest.started_at,
            ended_at=ended_at or datetime.now(UTC),
            duration_ms=duration_ms,
            status=status,
            error_type=error_type,
            error_message=error_message,
            attributes=latest.attributes,
            artifact_refs=latest.artifact_refs,
        )
        if span.duration_ms is None and span.ended_at is not None:
            delta = span.ended_at - span.started_at
            span = TraceSpan(
                **{
                    **span.to_dict(),
                    "duration_ms": max(0, int(delta.total_seconds() * 1000)),
                }
            )
        self.append_span(span)
        return span

    def append_artifact(self, artifact: TraceArtifact) -> None:
        self._append_jsonl(self.artifacts_path, artifact.to_dict())

    def query_events(
        self,
        *,
        run_id: str | None = None,
        session_id: str | None = None,
        task_id: str | None = None,
        event_type: TraceEventType | str | None = None,
        severity: TraceSeverity | str | None = None,
    ) -> list[TraceEvent]:
        events = self._read_events()
        if event_type is not None:
            event_type = TraceEventType(event_type)
        if severity is not None:
            severity = TraceSeverity(severity)
        return [
            event
            for event in events
            if (run_id is None or event.run_id == run_id)
            and (session_id is None or event.session_id == session_id)
            and (task_id is None or event.task_id == task_id)
            and (event_type is None or event.event_type == event_type)
            and (severity is None or event.severity == severity)
        ]

    def get_timeline(
        self,
        *,
        task_id: str | None = None,
        run_id: str | None = None,
        phase_id: str | None = None,
        action_id: str | None = None,
    ) -> list[TraceTimelineItem]:
        return TraceTimelineBuilder().build(
            self._read_events(),
            run_id=run_id,
            task_id=task_id,
            phase_id=phase_id,
            action_id=action_id,
        )

    def summarize(
        self,
        *,
        run_id: str | None = None,
        task_id: str | None = None,
    ) -> TraceSummary:
        return TraceSummaryBuilder().summarize(
            events=self._read_events(),
            spans=list(self.latest_spans().values()),
            artifacts=self._read_artifacts(),
            run_id=run_id,
            task_id=task_id,
        )

    def recover_incomplete_spans(self) -> list[str]:
        recovered: list[str] = []
        for span_id, span in self.latest_spans().items():
            if span.status == TraceStatus.RUNNING:
                self.update_span_end(
                    span_id,
                    TraceStatus.FAILED,
                    error_type="TraceRecoveredIncompleteSpan",
                    error_message="Span was running when trace store recovered.",
                )
                recovered.append(span_id)
        return recovered

    def latest_spans(self) -> dict[str, TraceSpan]:
        spans: dict[str, TraceSpan] = {}
        for span in self._read_spans():
            spans[span.span_id] = span
        return spans

    def artifacts(self) -> list[TraceArtifact]:
        return self._read_artifacts()

    def _append_jsonl(self, path: Path, payload: dict[str, Any]) -> None:
        try:
            path.parent.mkdir(parents=True, exist_ok=True)
            with path.open("a", encoding="utf-8") as file:
                file.write(json.dumps(payload, ensure_ascii=False, sort_keys=True, default=str) + "\n")
                file.flush()
                os.fsync(file.fileno())
        except OSError as exc:
            raise TraceStoreError(str(exc)) from exc

    def _read_events(self) -> list[TraceEvent]:
        return [TraceEvent.from_dict(item) for item in self._read_jsonl(self.events_path)]

    def _read_spans(self) -> list[TraceSpan]:
        return [TraceSpan.from_dict(item) for item in self._read_jsonl(self.spans_path)]

    def _read_artifacts(self) -> list[TraceArtifact]:
        return [TraceArtifact.from_dict(item) for item in self._read_jsonl(self.artifacts_path)]

    @staticmethod
    def _read_jsonl(path: Path) -> list[dict[str, Any]]:
        if not path.exists():
            return []
        rows: list[dict[str, Any]] = []
        for line in path.read_text(encoding="utf-8").splitlines():
            if line.strip():
                rows.append(json.loads(line))
        return rows

    def _write_index(self) -> None:
        payload = {
            "run_id": self.run_id,
            "events": "events.jsonl",
            "spans": "spans.jsonl",
            "artifacts": "artifacts.jsonl",
            "created_at": datetime.now(UTC).isoformat(),
        }
        if not self.index_path.exists():
            self._atomic_write_json(self.index_path, payload)

    @staticmethod
    def _atomic_write_json(path: Path, payload: dict[str, Any]) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp = path.with_name(f".{path.name}.{os.getpid()}.tmp")
        with tmp.open("w", encoding="utf-8", newline="\n") as file:
            file.write(json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True))
            file.flush()
            os.fsync(file.fileno())
        os.replace(tmp, path)
