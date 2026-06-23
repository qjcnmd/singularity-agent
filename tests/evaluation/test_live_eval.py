from __future__ import annotations

import json
import sys
from pathlib import Path

from singularity.evaluation.live import (
    LIVE_RESULT_SCHEMA_VERSION,
    LIVE_TASK_SET_SCHEMA_VERSION,
    LiveAgentEvalRunner,
    LiveEvalManifest,
    LiveEvalTaskResult,
    load_live_eval_manifest,
    summarize_live_results,
)
from singularity.kernel.finalization import FinalReport
from singularity.kernel.models import RunStatus


def test_load_live_eval_manifest_example() -> None:
    manifest = load_live_eval_manifest(Path("docs/evaluation/live-agent-minimal-tasks.json"))

    assert manifest.schema_version == LIVE_TASK_SET_SCHEMA_VERSION
    assert [task.task_id for task in manifest.tasks] == [
        "live.create_quicksort",
        "live.fix_math_test",
        "live.repair_slugger",
    ]
    assert manifest.tasks[0].workspace.kind == "fixture"
    assert manifest.tasks[0].allowed_paths == ["quicksort.py"]


def test_summarize_live_results_reports_cache_and_rates(tmp_path: Path) -> None:
    first = LiveEvalTaskResult(
        task_id="one",
        success=True,
        tests_passed=True,
        infrastructure_blocked=False,
        prompt_tokens=100,
        cached_tokens=25,
        request_cache_hit_rate=0.25,
        run_cache_hit_rate=0.25,
        tool_calls=2,
        files_changed=["a.py"],
        duration_seconds=1.0,
        error_summary="",
        workspace=str(tmp_path),
        trace=str(tmp_path / "trace"),
    )
    second = LiveEvalTaskResult(
        task_id="two",
        success=False,
        tests_passed=True,
        infrastructure_blocked=False,
        prompt_tokens=100,
        cached_tokens=75,
        request_cache_hit_rate=0.75,
        run_cache_hit_rate=0.75,
        tool_calls=3,
        files_changed=["b.py"],
        duration_seconds=2.0,
        error_summary="agent status: failed",
        workspace=str(tmp_path),
        trace=str(tmp_path / "trace2"),
    )
    blocked = LiveEvalTaskResult(
        task_id="blocked",
        success=False,
        tests_passed=False,
        infrastructure_blocked=True,
        prompt_tokens=0,
        cached_tokens=0,
        request_cache_hit_rate=0.0,
        run_cache_hit_rate=0.0,
        tool_calls=0,
        files_changed=[],
        duration_seconds=0.5,
        error_summary="infrastructure blocked",
        workspace=str(tmp_path),
        trace=str(tmp_path / "trace3"),
    )

    summary = summarize_live_results([first, second, blocked])

    assert summary == {
        "task_count": 3,
        "scored_task_count": 2,
        "infrastructure_blocked_count": 1,
        "score_status": "scored",
        "success_count": 1,
        "task_completion_rate": 0.5,
        "tests_passed_count": 2,
        "test_pass_rate": 1.0,
        "prompt_tokens": 200,
        "cached_tokens": 100,
        "request_cache_hit_rate": 0.5,
        "run_cache_hit_rate": 0.5,
        "tool_calls": 5,
    }


def test_live_eval_runner_writes_result_without_live_provider(tmp_path: Path) -> None:
    py = json.dumps(sys.executable)
    manifest_payload = {
        "schema_version": LIVE_TASK_SET_SCHEMA_VERSION,
        "tasks": [
            {
                "task_id": "fake.write_file",
                "workspace": {"type": "fixture", "files": {"README.md": "fixture\n"}},
                "prepare_commands": [f"{py} -c \"print('ready')\""],
                "user_task": "Write done.txt with ok.",
                "allowed_paths": ["done.txt"],
                "verification_command": f"{py} -c \"from pathlib import Path; assert Path('done.txt').read_text(encoding='utf-8') == 'ok'\"",
                "success": {"type": "verification_exit_code", "exit_code": 0},
            }
        ],
    }
    manifest = LiveEvalManifest.from_dict(manifest_payload, base_dir=tmp_path)

    class FakeTraceStore:
        run_dir = tmp_path / "trace"

    class FakeTrace:
        store = FakeTraceStore()

    class FakeGraph:
        trace = FakeTrace()

    class FakeResult:
        status = RunStatus.COMPLETED
        final_report = FinalReport(
            run_id="run_1",
            session_id="session_1",
            task_id="task_1",
            kernel_status="finalized",
            shutdown_reason="normal",
            diagnostics_count=0,
            cleanup_status="completed",
            recovered_previous_run=False,
            uncertain_transactions=[],
            workspace_lock_status="released",
            trace_summary={
                "tool_calls": 2,
                "model_usage_summary": {
                    "input_tokens": 10,
                    "cached_input_tokens": 4,
                    "request_cache_hit_rates": {"req_1": 0.4},
                    "run_cache_hit_rate": 0.4,
                },
            },
        )

    class FakeKernel:
        graph = FakeGraph()

        def __init__(self, project_root: Path) -> None:
            self.project_root = project_root

        def run_task(self, _goal: str) -> FakeResult:
            (self.project_root / "done.txt").write_text("ok", encoding="utf-8")
            return FakeResult()

        def close_resources(self) -> None:
            return None

    class FakeBootstrap:
        def __init__(self, **kwargs) -> None:
            self.project_root = kwargs["project_root"]

        def boot(self, _goal: str) -> FakeKernel:
            return FakeKernel(self.project_root)

    result = LiveAgentEvalRunner(
        output_root=tmp_path / "out",
        run_id="run_fake",
        bootstrap_cls=FakeBootstrap,
    ).run(manifest)

    assert result["schema_version"] == LIVE_RESULT_SCHEMA_VERSION
    assert result["summary"]["success_count"] == 1
    task = result["tasks"][0]
    assert task["task_id"] == "fake.write_file"
    assert task["success"] is True
    assert task["tests_passed"] is True
    assert task["prompt_tokens"] == 10
    assert task["cached_tokens"] == 4
    assert task["request_cache_hit_rate"] == 0.4
    assert task["run_cache_hit_rate"] == 0.4
    assert task["tool_calls"] == 2
    assert task["files_changed"] == ["done.txt"]
    assert Path(result["result_path"]).exists()


def test_live_eval_marks_model_transport_blocker_without_running_verification(tmp_path: Path) -> None:
    py = json.dumps(sys.executable)
    manifest = LiveEvalManifest.from_dict(
        {
            "schema_version": LIVE_TASK_SET_SCHEMA_VERSION,
            "tasks": [
                {
                    "task_id": "fake.model_blocked",
                    "workspace": {"type": "fixture", "files": {"README.md": "fixture\n"}},
                    "user_task": "Write done.txt with ok.",
                    "allowed_paths": ["done.txt"],
                    "verification_command": f"{py} -c \"raise SystemExit(99)\"",
                    "success": {"type": "verification_exit_code", "exit_code": 0},
                }
            ],
        },
        base_dir=tmp_path,
    )

    class FakeTraceStore:
        run_dir = tmp_path / "trace"

    class FakeTrace:
        store = FakeTraceStore()

    class FakeGraph:
        trace = FakeTrace()

    class FakeResult:
        status = RunStatus.FAILED
        final_answer = "[WinError 10013] socket access denied"
        final_report = FinalReport(
            run_id="run_1",
            session_id="session_1",
            task_id="task_1",
            kernel_status="finalized",
            shutdown_reason="error",
            diagnostics_count=1,
            cleanup_status="completed",
            recovered_previous_run=False,
            uncertain_transactions=[],
            workspace_lock_status="released",
            trace_summary={"model_usage_summary": {"input_tokens": 0}},
        )

    class FakeKernel:
        graph = FakeGraph()

        def run_task(self, _goal: str) -> FakeResult:
            return FakeResult()

        def close_resources(self) -> None:
            return None

    class FakeBootstrap:
        def __init__(self, **kwargs) -> None:
            _ = kwargs

        def boot(self, _goal: str) -> FakeKernel:
            return FakeKernel()

    result = LiveAgentEvalRunner(
        output_root=tmp_path / "out",
        run_id="run_blocked",
        bootstrap_cls=FakeBootstrap,
    ).run(manifest)

    assert result["summary"]["infrastructure_blocked_count"] == 1
    assert result["summary"]["scored_task_count"] == 0
    assert result["summary"]["score_status"] == "infrastructure_blocked"
    assert result["summary"]["task_completion_rate"] == 0.0
    assert result["summary"]["test_pass_rate"] == 0.0
    task = result["tasks"][0]
    assert task["infrastructure_blocked"] is True
    assert task["verification"] is None
    assert "infrastructure blocked" in task["error_summary"]


def test_live_eval_runs_hidden_verification_prepare_after_agent(tmp_path: Path) -> None:
    py = json.dumps(sys.executable)
    hidden_test = "from solution import answer\n\n\ndef test_answer():\n    assert answer() == 42\n"
    hidden_source = tmp_path / "hidden_test_source.py"
    hidden_source.write_text(hidden_test, encoding="utf-8")
    hidden_code = (
        "from pathlib import Path; "
        "Path('tests').mkdir(exist_ok=True); "
        f"Path('tests/test_hidden.py').write_text(Path({str(hidden_source)!r}).read_text(encoding='utf-8'), encoding='utf-8')"
    )
    manifest = LiveEvalManifest.from_dict(
        {
            "schema_version": LIVE_TASK_SET_SCHEMA_VERSION,
            "tasks": [
                {
                    "task_id": "fake.hidden_verification",
                    "workspace": {"type": "fixture", "files": {"README.md": "fixture\n"}},
                    "user_task": "Create solution.py with answer().",
                    "allowed_paths": ["solution.py"],
                    "verification_prepare_commands": [f"{py} -c {json.dumps(hidden_code)}"],
                    "verification_command": f"{py} -m pytest tests/test_hidden.py",
                    "success": {"type": "verification_exit_code", "exit_code": 0},
                }
            ],
        },
        base_dir=tmp_path,
    )
    seen_goals: list[str] = []

    class FakeTraceStore:
        run_dir = tmp_path / "trace"

    class FakeTrace:
        store = FakeTraceStore()

    class FakeGraph:
        trace = FakeTrace()

    class FakeResult:
        status = RunStatus.COMPLETED
        final_report = FinalReport(
            run_id="run_1",
            session_id="session_1",
            task_id="task_1",
            kernel_status="finalized",
            shutdown_reason="normal",
            diagnostics_count=0,
            cleanup_status="completed",
            recovered_previous_run=False,
            uncertain_transactions=[],
            workspace_lock_status="released",
            trace_summary={"tool_calls": 1, "model_usage_summary": {"input_tokens": 10}},
        )

    class FakeKernel:
        graph = FakeGraph()

        def __init__(self, project_root: Path) -> None:
            self.project_root = project_root

        def run_task(self, goal: str) -> FakeResult:
            seen_goals.append(goal)
            (self.project_root / "solution.py").write_text("def answer():\n    return 42\n", encoding="utf-8")
            return FakeResult()

        def close_resources(self) -> None:
            return None

    class FakeBootstrap:
        def __init__(self, **kwargs) -> None:
            self.project_root = kwargs["project_root"]

        def boot(self, _goal: str) -> FakeKernel:
            return FakeKernel(self.project_root)

    result = LiveAgentEvalRunner(
        output_root=tmp_path / "out",
        run_id="run_hidden",
        bootstrap_cls=FakeBootstrap,
    ).run(manifest)

    assert result["summary"]["success_count"] == 1
    task = result["tasks"][0]
    assert task["success"] is True
    assert task["files_changed"] == ["solution.py"]
    assert "tests/test_hidden.py" not in seen_goals[0]
