from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from singularity.evaluation.models import SCHEMA_VERSION, BenchmarkTask

TASK_SET_SCHEMA_VERSION = "evaluation.golden_task_set/v1"


class GoldenTaskStore:
    def __init__(self, path: Path | str) -> None:
        self.path = Path(path)

    def load(
        self,
        *,
        version: str | None = None,
        tags: list[str] | None = None,
    ) -> list[BenchmarkTask]:
        payload = self._read_document()
        tasks_payload = payload.get("tasks", [])
        tasks = [BenchmarkTask.from_dict(item) for item in tasks_payload]
        if version is not None:
            tasks = [task for task in tasks if task.version == version]
        if tags:
            required_tags = set(tags)
            tasks = [task for task in tasks if required_tags.issubset(set(task.tags))]
        return sorted(tasks, key=lambda task: (task.version, task.task_id))

    def save(self, tasks: list[BenchmarkTask]) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        text = self.to_json_document(tasks)
        if self.path.suffix.lower() in {".yaml", ".yml"}:
            try:
                import yaml
            except ImportError as exc:
                raise RuntimeError(
                    "YAML golden task storage requires PyYAML>=6; JSON storage is always available."
                ) from exc
            payload = json.loads(text)
            self.path.write_text(
                yaml.safe_dump(payload, sort_keys=True, allow_unicode=True),
                encoding="utf-8",
            )
            return
        self.path.write_text(text, encoding="utf-8")

    def validate(self) -> list[BenchmarkTask]:
        return self.load()

    @staticmethod
    def to_json_document(tasks: list[BenchmarkTask]) -> str:
        payload = {
            "schema_version": TASK_SET_SCHEMA_VERSION,
            "task_schema_version": SCHEMA_VERSION,
            "tasks": [task.to_dict() for task in sorted(tasks, key=lambda item: item.task_id)],
        }
        return json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True) + "\n"

    def _read_document(self) -> dict[str, Any]:
        if not self.path.exists():
            raise FileNotFoundError(self.path)
        if self.path.suffix.lower() in {".yaml", ".yml"}:
            try:
                import yaml
            except ImportError as exc:
                raise RuntimeError(
                    "YAML golden task storage requires PyYAML>=6; JSON storage is always available."
                ) from exc
            loaded = yaml.safe_load(self.path.read_text(encoding="utf-8"))
            return self._validate_document(loaded or {"tasks": []})
        return self._validate_document(json.loads(self.path.read_text(encoding="utf-8")))

    @staticmethod
    def _validate_document(payload: dict[str, Any]) -> dict[str, Any]:
        schema_version = payload.get("schema_version")
        task_schema_version = payload.get("task_schema_version")
        if schema_version != TASK_SET_SCHEMA_VERSION:
            raise ValueError(f"Unsupported GoldenTaskSet.schema_version: {schema_version}")
        if task_schema_version != SCHEMA_VERSION:
            raise ValueError(
                f"Unsupported GoldenTaskSet.task_schema_version: {task_schema_version}"
            )
        tasks = payload.get("tasks", [])
        if not isinstance(tasks, list):
            raise ValueError("GoldenTaskSet.tasks must be a list.")
        return payload
