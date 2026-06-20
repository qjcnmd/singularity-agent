from __future__ import annotations

from pathlib import Path

import pytest

from miniharness.evaluation import (
    BenchmarkTask,
    EvaluationHook,
    ExpectedOutcome,
    ExpectedOutcomeKind,
    GoldenTaskStore,
    TaskDifficulty,
    WorkspaceSnapshot,
    WorkspaceSnapshotKind,
)
from miniharness.evaluation.models import SCHEMA_VERSION
from miniharness.evaluation.store import TASK_SET_SCHEMA_VERSION


def _task(task_id: str = "task.schema") -> BenchmarkTask:
    return BenchmarkTask(
        task_id=task_id,
        version="v1",
        title="Fix parser edge case",
        input_prompt="Fix parsing for empty sections.",
        workspace_snapshot=WorkspaceSnapshot(
            kind=WorkspaceSnapshotKind.INLINE_FILES,
            inline_files={"parser.py": "def parse(text):\n    return []\n"},
        ),
        expected_outcomes=[
            ExpectedOutcome(
                kind=ExpectedOutcomeKind.TEST,
                weight=0.7,
                command="python -m pytest tests/test_parser.py",
            ),
            ExpectedOutcome(
                kind=ExpectedOutcomeKind.HEURISTIC,
                weight=0.3,
                heuristic="patch_quality",
            ),
        ],
        evaluation_hooks=[
            EvaluationHook(name="prep", stage="before_run", command="python -m compileall src")
        ],
        tags=[TaskDifficulty.EASY.value, "memory-heavy"],
        profiles={"model": "cheap", "tool_policy": "read_write"},
    )


def test_benchmark_task_round_trips_and_validates_required_schema() -> None:
    task = _task()

    restored = BenchmarkTask.from_dict(task.to_dict())

    assert restored == task
    assert restored.input.prompt == "Fix parsing for empty sections."
    assert restored.workspace_snapshot.kind == WorkspaceSnapshotKind.INLINE_FILES
    assert restored.expected_outcomes[0].kind == ExpectedOutcomeKind.TEST
    assert restored.tags == ["easy", "memory-heavy"]


def test_task_validation_rejects_missing_prompt_and_invalid_tag() -> None:
    with pytest.raises(ValueError, match="input.prompt"):
        BenchmarkTask.from_dict(
            {
                "task_id": "bad",
                "version": "v1",
                "input": {"prompt": ""},
                "workspace_snapshot": {"kind": "git_ref", "git_ref": "HEAD"},
                "expected_outcomes": [{"kind": "test", "weight": 1.0}],
                "tags": ["easy"],
            }
        )

    with pytest.raises(ValueError, match="difficulty tag"):
        _task().with_updates(tags=["tiny"])


def test_task_validation_rejects_unknown_schema_version() -> None:
    payload = _task().to_dict()
    payload["schema_version"] = "evaluation.benchmark_task/v999"

    with pytest.raises(ValueError, match="schema_version"):
        BenchmarkTask.from_dict(payload)


def test_golden_task_store_loads_json_and_filters_by_version_and_tags(tmp_path: Path) -> None:
    store_path = tmp_path / "golden.json"
    store_path.write_text(
        GoldenTaskStore.to_json_document(
            [
                _task("task.easy.v1"),
                _task("task.hard.v2").with_updates(
                    version="v2",
                    tags=[TaskDifficulty.HARD.value, "tool-heavy"],
                ),
            ]
        ),
        encoding="utf-8",
    )

    store = GoldenTaskStore(store_path)
    selected = store.load(version="v2", tags=["tool-heavy"])

    assert [task.task_id for task in selected] == ["task.hard.v2"]


def test_golden_task_store_rejects_unknown_document_versions(tmp_path: Path) -> None:
    store_path = tmp_path / "golden.json"
    store_path.write_text(
        (
            GoldenTaskStore.to_json_document([_task("task.version")])
            .replace(TASK_SET_SCHEMA_VERSION, "evaluation.golden_task_set/v999")
            .replace(SCHEMA_VERSION, "evaluation.benchmark_task/v999")
        ),
        encoding="utf-8",
    )

    with pytest.raises(ValueError, match="GoldenTaskSet.schema_version"):
        GoldenTaskStore(store_path).load()


def test_golden_task_store_writes_yaml_when_pyyaml_is_available(tmp_path: Path) -> None:
    pytest.importorskip("yaml")
    store_path = tmp_path / "golden.yaml"
    store = GoldenTaskStore(store_path)

    store.save([_task("task.yaml")])

    loaded = store.load(tags=["memory-heavy"])
    assert [task.task_id for task in loaded] == ["task.yaml"]
