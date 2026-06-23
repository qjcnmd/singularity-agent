from __future__ import annotations

from pathlib import Path

import pytest

from singularity.evaluation import (
    BenchmarkTask,
    BenchmarkAdapterKind,
    BenchmarkTaskKind,
    BenchmarkVisibility,
    EvaluationHook,
    ExpectedOutcome,
    ExpectedOutcomeKind,
    GoldenTaskStore,
    TaskDifficulty,
    WorkspaceSnapshot,
    WorkspaceSnapshotKind,
)
from singularity.evaluation.models import SCHEMA_VERSION
from singularity.evaluation.store import TASK_SET_SCHEMA_VERSION

ROOT = Path(__file__).resolve().parents[2]
PHASE1J_TASK_SET = ROOT / "docs" / "evaluation" / "phase1j-golden-tasks.json"


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
    assert restored.task_type == BenchmarkTaskKind.SINGULARITY_INTERNAL
    assert restored.visibility == BenchmarkVisibility.PRIVATE
    assert restored.adapter == BenchmarkAdapterKind.SINGULARITY_PRIVATE
    assert restored.input.prompt == "Fix parsing for empty sections."
    assert restored.workspace_snapshot.kind == WorkspaceSnapshotKind.INLINE_FILES
    assert restored.expected_outcomes[0].kind == ExpectedOutcomeKind.TEST
    assert restored.tags == ["easy", "memory-heavy"]


def test_benchmark_task_supports_public_repo_issue_and_terminal_task_schema() -> None:
    repo_issue = _task("task.repo_issue").with_updates(
        task_type="repo_issue_repair",
        visibility="public",
        adapter="swe_bench",
        input={
            "prompt": "Fix the issue and produce a patch.",
            "metadata": {
                "repo": "owner/project",
                "base_commit": "abc123",
                "issue": "Parser drops empty sections.",
            },
        },
    )
    terminal = _task("task.terminal").with_updates(
        task_type="terminal_task",
        visibility="private",
        adapter="terminal_bench",
    )

    assert repo_issue.task_type == BenchmarkTaskKind.REPO_ISSUE_REPAIR
    assert repo_issue.visibility == BenchmarkVisibility.PUBLIC
    assert repo_issue.adapter == BenchmarkAdapterKind.SWE_BENCH
    assert repo_issue.input.metadata["base_commit"] == "abc123"
    assert terminal.task_type == BenchmarkTaskKind.TERMINAL_TASK
    assert terminal.adapter == BenchmarkAdapterKind.TERMINAL_BENCH


def test_benchmark_task_rejects_unknown_type_visibility_or_adapter() -> None:
    payload = _task().to_dict()
    payload["task_type"] = "unknown"
    with pytest.raises(ValueError, match="BenchmarkTask.task_type"):
        BenchmarkTask.from_dict(payload)

    payload = _task().to_dict()
    payload["visibility"] = "secret"
    with pytest.raises(ValueError, match="BenchmarkTask.visibility"):
        BenchmarkTask.from_dict(payload)

    payload = _task().to_dict()
    payload["adapter"] = "random"
    with pytest.raises(ValueError, match="BenchmarkTask.adapter"):
        BenchmarkTask.from_dict(payload)


def test_benchmark_task_round_trips_golden_contract() -> None:
    payload = _task("task.contract").to_dict()
    payload["golden_contract"] = {
        "scenario": "create_file_smoke_verify",
        "expected_files": ["quicksort.py", "tests/test_quicksort.py"],
        "expected_commands": ["python -m pytest tests/test_quicksort.py"],
        "expected_evidence": ["file_created", "verification_passed"],
        "expected_report_sections": ["Goal", "Changes", "Verification", "Risks"],
        "required_trace_artifacts": ["diff", "verification", "report"],
    }

    restored = BenchmarkTask.from_dict(payload)
    round_tripped = restored.to_dict()

    assert "golden_contract" in round_tripped
    assert round_tripped["golden_contract"]["scenario"] == "create_file_smoke_verify"
    assert round_tripped["golden_contract"]["expected_files"] == [
        "quicksort.py",
        "tests/test_quicksort.py",
    ]
    assert round_tripped["golden_contract"]["required_trace_artifacts"] == [
        "diff",
        "verification",
        "report",
    ]


def test_phase1j_golden_task_set_covers_all_required_scenarios() -> None:
    assert PHASE1J_TASK_SET.exists(), "Phase 1J golden task set must be checked in."

    tasks = GoldenTaskStore(PHASE1J_TASK_SET).load(tags=["phase1j-golden"])
    task_ids = {task.task_id for task in tasks}

    assert task_ids == {
        "phase1j.create_file_smoke_verify",
        "phase1j.modify_bug_test_pass",
        "phase1j.verification_failure_repair",
        "phase1j.completion_rejected_continue",
        "phase1j.final_review_rejected_repair",
        "phase1j.full_markdown_report",
        "phase1j.approval_required_resume",
        "phase1j.sandbox_required_unavailable_fail_closed",
        "phase1j.dynamic_retrieval_after_failure",
        "phase1j.memory_write_after_verified_completion",
    }
    for task in tasks:
        contract = task.to_dict().get("golden_contract", {})
        assert contract.get("expected_files"), task.task_id
        assert contract.get("expected_commands"), task.task_id
        assert contract.get("expected_evidence"), task.task_id
        assert contract.get("expected_report_sections"), task.task_id
        assert contract.get("required_trace_artifacts"), task.task_id
        assert any(outcome.kind != ExpectedOutcomeKind.HEURISTIC for outcome in task.expected_outcomes)


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
