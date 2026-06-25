from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Any

from singularity.evaluation.live import (
    LIVE_RESULT_SCHEMA_VERSION,
    LIVE_TASK_SET_SCHEMA_VERSION,
    LiveAgentEvalRunner,
    LiveEvalManifest,
    LiveEvalTaskResult,
    SingularityPrivateBenchmarkAdapter,
    load_live_eval_manifest,
    summarize_live_results,
)
from singularity.kernel.finalization import FinalReport
from singularity.kernel.models import RunStatus
from tests.agent_loop_helpers import make_agent_session


def test_load_live_eval_manifest_example() -> None:
    manifest = load_live_eval_manifest(Path("docs/evaluation/live-agent-minimal-tasks.json"))

    assert manifest.schema_version == LIVE_TASK_SET_SCHEMA_VERSION
    assert [task.task_id for task in manifest.tasks] == [
        "live.create_quicksort",
        "live.modify_existing_code",
        "live.fix_math_test",
        "live.reject_out_of_scope_change",
        "live.verification_contract",
        "live.completion_rejected_repair",
        "live.policy_blocked",
    ]
    assert manifest.tasks[0].workspace.kind == "fixture"
    assert manifest.tasks[0].allowed_paths == ["quicksort.py"]
    assert manifest.tasks[0].expected_file_changes == ["quicksort.py"]
    assert manifest.tasks[0].completion_standard
    assert "smoke-test" in manifest.tasks[0].risk_tags
    assert manifest.tasks[-1].tool_policy == "read_only"


def test_private_adapter_converts_benchmark_tasks_to_live_manifest(tmp_path: Path) -> None:
    task_set = tmp_path / "private.json"
    task_set.write_text(
        json.dumps(
            {
                "schema_version": "evaluation.golden_task_set/v1",
                "task_schema_version": "evaluation.benchmark_task/v1",
                "tasks": [
                    {
                        "schema_version": "evaluation.benchmark_task/v1",
                        "task_id": "private.fix_bug",
                        "version": "v1",
                        "title": "Fix bug",
                        "task_type": "repo_issue_repair",
                        "visibility": "private",
                        "adapter": "singularity_private",
                        "input": {
                            "prompt": "Fix math_utils.py.",
                            "metadata": {"allowed_paths": ["math_utils.py"]},
                        },
                        "workspace_snapshot": {
                            "kind": "inline_files",
                            "inline_files": {"math_utils.py": "def add(a, b):\n    return a - b\n"},
                        },
                        "allowed_tools": ["read_file", "write_file", "run_verification"],
                        "strategy": {"tool_policy": "read_write", "approval_mode": "auto_safe"},
                        "expected_file_changes": ["math_utils.py"],
                        "completion_standard": "Focused pytest passes.",
                        "risk_tags": ["test-repair"],
                        "expected_outcomes": [
                            {"kind": "test", "weight": 1.0, "command": f"{json.dumps(sys.executable)} -m pytest tests/test_math.py"}
                        ],
                        "tags": ["easy"],
                    },
                    {
                        "schema_version": "evaluation.benchmark_task/v1",
                        "task_id": "private.repo_issue",
                        "version": "v1",
                        "title": "Fix repo bug",
                        "task_type": "repo_issue_repair",
                        "visibility": "private",
                        "adapter": "singularity_private",
                        "input": {
                            "prompt": "Fix the repo issue.",
                            "metadata": {"repo_path": str(tmp_path / "repo"), "allowed_paths": ["src"]},
                        },
                        "workspace_snapshot": {"kind": "git_ref", "git_ref": "abc123"},
                        "expected_outcomes": [
                            {"kind": "test", "weight": 1.0, "command": "python -m pytest"}
                        ],
                        "tags": ["medium"],
                    }
                ],
            },
            indent=2,
        ),
        encoding="utf-8",
    )

    manifest = SingularityPrivateBenchmarkAdapter().load(task_set)

    assert manifest.schema_version == LIVE_TASK_SET_SCHEMA_VERSION
    assert manifest.tasks[0].task_id == "private.fix_bug"
    assert manifest.tasks[0].workspace.kind == "fixture"
    assert manifest.tasks[0].allowed_paths == ["math_utils.py"]
    assert manifest.tasks[0].allowed_tools == ["read_file", "run_verification", "write_file"]
    assert manifest.tasks[0].expected_file_changes == ["math_utils.py"]
    assert manifest.tasks[0].completion_standard == "Focused pytest passes."
    assert manifest.tasks[0].risk_tags == ["test-repair"]
    assert manifest.tasks[0].verification_command == f"{json.dumps(sys.executable)} -m pytest tests/test_math.py"
    assert manifest.tasks[1].task_id == "private.repo_issue"
    assert manifest.tasks[1].workspace.kind == "repo"
    assert manifest.tasks[1].workspace.path == str(tmp_path / "repo")
    assert manifest.tasks[1].workspace.start_commit == "abc123"


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
        status="success",
        turn_count=2,
        final_report_status="completed",
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
        status="verification_failed",
        turn_count=3,
        final_report_status="completed",
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
        status="infrastructure_blocked",
        turn_count=0,
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
        "success_rate": 0.5,
        "verification_pass_rate": 1.0,
        "average_turns": 2.5,
        "average_tool_calls": 2.5,
        "failure_repair_count": 0,
        "policy_blocks": 0,
        "miscompletion_count": 1,
        "failure_reasons": {"verification_failed": 1},
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
                    "requests": 2,
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
    assert task["status"] == "success"
    assert task["turn_count"] == 2
    assert task["tool_calls"] == 2
    assert task["files_changed"] == ["done.txt"]
    assert task["patch"]["applicable"] is True
    assert "done.txt" in task["patch"]["diff"]
    assert task["checks"]["public"]["passed"] is True
    assert task["verification_result"]["status"] == "passed"
    assert task["contract_satisfaction"]["status"] == "satisfied"
    assert task["final_report_status"] == "finalized"
    assert task["token_usage"]["input_tokens"] == 10
    assert task["cache_usage"]["run_cache_hit_rate"] == 0.4
    assert task["agent_loop_ref"].endswith("AgentLoop.run")
    assert Path(result["result_path"]).exists()
    assert Path(result["report_path"]).exists()
    assert Path(result["markdown_path"]).exists()
    assert "Live Agent Evaluation" in Path(result["markdown_path"]).read_text(encoding="utf-8")


def test_live_eval_runner_compares_against_previous_run(tmp_path: Path) -> None:
    py = json.dumps(sys.executable)
    manifest = LiveEvalManifest.from_dict(
        {
            "schema_version": LIVE_TASK_SET_SCHEMA_VERSION,
            "tasks": [
                {
                    "task_id": "fake.regression",
                    "workspace": {"type": "fixture", "files": {"README.md": "fixture\n"}},
                    "user_task": "Write done.txt with ok.",
                    "allowed_paths": ["done.txt"],
                    "expected_file_changes": ["done.txt"],
                    "verification_command": f"{py} -c \"from pathlib import Path; assert Path('done.txt').read_text(encoding='utf-8') == 'ok'\"",
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
            trace_summary={"tool_calls": 1, "model_usage_summary": {"requests": 1, "input_tokens": 10}},
        )

    class FakeKernel:
        graph = FakeGraph()

        def __init__(self, project_root: Path, should_write: bool) -> None:
            self.project_root = project_root
            self.should_write = should_write

        def run_task(self, _goal: str) -> FakeResult:
            if self.should_write:
                (self.project_root / "done.txt").write_text("ok", encoding="utf-8")
            return FakeResult()

        def close_resources(self) -> None:
            return None

    class FakeBootstrap:
        should_write = True

        def __init__(self, **kwargs) -> None:
            self.project_root = kwargs["project_root"]

        def boot(self, _goal: str) -> FakeKernel:
            return FakeKernel(self.project_root, self.should_write)

    output_root = tmp_path / "out"
    first = LiveAgentEvalRunner(
        output_root=output_root,
        run_id="baseline",
        bootstrap_cls=FakeBootstrap,
    ).run(manifest)
    FakeBootstrap.should_write = False
    second = LiveAgentEvalRunner(
        output_root=output_root,
        run_id="candidate",
        bootstrap_cls=FakeBootstrap,
    ).run(manifest)

    assert first["summary"]["success_count"] == 1
    assert second["summary"]["success_count"] == 0
    assert second["regression"]["summary"]["regression_count"] == 1
    assert second["regression"]["task_diffs"][0]["task_id"] == "fake.regression"
    assert Path(second["regression_path"]).exists()
    assert Path(second["regression_markdown_path"]).exists()


def test_live_eval_runner_can_drive_real_agent_loop(tmp_path: Path) -> None:
    py = json.dumps(sys.executable)
    manifest = LiveEvalManifest.from_dict(
        {
            "schema_version": LIVE_TASK_SET_SCHEMA_VERSION,
            "tasks": [
                {
                    "task_id": "fake.real_agent_loop",
                    "workspace": {"type": "fixture", "files": {"README.md": "fixture\n"}},
                    "user_task": "Say done without modifying files.",
                    "allowed_paths": ["."],
                    "verification_command": f"{py} -c \"print('ok')\"",
                    "success": {"type": "verification_exit_code", "exit_code": 0},
                }
            ],
        },
        base_dir=tmp_path,
    )
    calls: list[str] = []

    class MockProvider:
        def chat(self, *, messages: list[dict[str, Any]], tools: list[dict[str, Any]]) -> dict[str, Any]:
            _ = messages, tools
            calls.append("chat")
            return {"choices": [{"message": {"role": "assistant", "content": "done"}}]}

    class FakeKernel:
        graph = None

        def __init__(self, project_root: Path) -> None:
            self.project_root = project_root

        def run_task(self, goal: str):
            agent = make_agent_session(
                self.project_root,
                provider=MockProvider(),
                max_turns=1,
            )
            agent_result = agent.run(goal)

            class FakeGraph:
                trace = agent.trace

            self.graph = FakeGraph()

            class Result:
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
                        trace_summary={"tool_calls": 0, "model_usage_summary": {"requests": 1, "input_tokens": 1}},
                    )
                final_answer = agent_result.final_answer

            return Result()

        def close_resources(self) -> None:
            return None

    class FakeBootstrap:
        def __init__(self, **kwargs) -> None:
            self.project_root = kwargs["project_root"]

        def boot(self, _goal: str) -> FakeKernel:
            return FakeKernel(self.project_root)

    result = LiveAgentEvalRunner(
        output_root=tmp_path / "out",
        run_id="real_loop",
        bootstrap_cls=FakeBootstrap,
    ).run(manifest)

    assert calls == ["chat"]
    assert result["tasks"][0]["success"] is True
    assert result["tasks"][0]["agent_loop_ref"] == "KernelBootstrap.boot -> AgentKernel.run_task -> AgentLoop.run"


def test_live_eval_prepare_failure_returns_structured_result(tmp_path: Path) -> None:
    py = json.dumps(sys.executable)
    manifest = LiveEvalManifest.from_dict(
        {
            "schema_version": LIVE_TASK_SET_SCHEMA_VERSION,
            "tasks": [
                {
                    "task_id": "fake.prepare_failure",
                    "workspace": {"type": "fixture", "files": {"README.md": "fixture\n"}},
                    "prepare_commands": [f"{py} -c \"raise SystemExit(3)\""],
                    "user_task": "Write done.txt.",
                    "allowed_paths": ["done.txt"],
                    "verification_command": f"{py} -c \"print('unused')\"",
                    "success": {"type": "verification_exit_code", "exit_code": 0},
                }
            ],
        },
        base_dir=tmp_path,
    )

    result = LiveAgentEvalRunner(
        output_root=tmp_path / "out",
        run_id="run_prepare_failed",
        bootstrap_cls=object,
    ).run(manifest)

    task = result["tasks"][0]
    assert task["success"] is False
    assert task["verification"]["exit_code"] == 3
    assert task["patch"]["applicable"] is False
    assert task["checks"]["hidden"]["status"] == "failed"
    assert "prepare failed" in task["error_summary"]


def test_live_eval_applies_patch_in_clean_verification_workspace(tmp_path: Path) -> None:
    py = json.dumps(sys.executable)
    manifest = LiveEvalManifest.from_dict(
        {
            "schema_version": LIVE_TASK_SET_SCHEMA_VERSION,
            "tasks": [
                {
                    "task_id": "fake.clean_apply",
                    "workspace": {"type": "fixture", "files": {"solution.py": "def answer():\n    return 0\n"}},
                    "user_task": "Fix solution.answer.",
                    "allowed_paths": ["solution.py"],
                    "verification_prepare_commands": [
                        f"{py} -c \"from pathlib import Path; Path('tests').mkdir(exist_ok=True); Path('tests/test_solution.py').write_text('from solution import answer\\n\\ndef test_answer():\\n    assert answer() == 42\\n', encoding='utf-8')\""
                    ],
                    "verification_command": f"{py} -m pytest tests/test_solution.py",
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

        def run_task(self, _goal: str) -> FakeResult:
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
        run_id="run_apply",
        bootstrap_cls=FakeBootstrap,
    ).run(manifest)

    task = result["tasks"][0]
    assert task["success"] is True
    assert task["workspace"] != task["verification_workspace"]
    assert Path(task["verification_workspace"], "tests", "test_solution.py").exists()
    assert task["patch"]["applicable"] is True
    assert task["checks"]["hidden"]["passed"] is True
    assert task["checks"]["public"]["passed"] is True


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
