from __future__ import annotations

import json
from datetime import UTC, datetime
from pathlib import Path
from uuid import uuid4

from singularity.observability.redaction import shared_trace_redactor


class JsonlTraceRecorder:
    def __init__(self, *, run_id: str, path: Path) -> None:
        self.run_id = run_id
        self.path = path

    @classmethod
    def create(cls, project_root: Path) -> JsonlTraceRecorder:
        run_id = cls._new_run_id()
        trace_dir = project_root / ".singularity" / "runs"
        trace_dir.mkdir(parents=True, exist_ok=True)
        return cls(run_id=run_id, path=trace_dir / f"{run_id}.jsonl")

    def record(self, event: str, data: dict) -> None:
        redactor = shared_trace_redactor()
        self.path.parent.mkdir(parents=True, exist_ok=True)
        entry = {
            "ts": datetime.now(UTC).isoformat(),
            "run_id": self.run_id,
            "event": event,
            "data": redactor.redact_payload(data),
        }
        with self.path.open("a", encoding="utf-8") as file:
            file.write(json.dumps(entry, ensure_ascii=False) + "\n")

    @staticmethod
    def _new_run_id() -> str:
        timestamp = datetime.now(UTC).strftime("%Y%m%dT%H%M%SZ")
        suffix = uuid4().hex[:8]
        return f"{timestamp}-{suffix}"
