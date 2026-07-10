import json
import os
from hashlib import sha256
from pathlib import Path

import pytest
from typer.main import get_command
from typer.testing import CliRunner

from singularity.evaluation import (
    BenchmarkTask,
    ExpectedOutcome,
    ExpectedOutcomeKind,
    GoldenTaskStore,
    WorkspaceSnapshot,
    WorkspaceSnapshotKind,
)
from singularity.kernel import CancellationError
from singularity.kernel.finalization import FinalReport
from singularity.kernel.models import RunStatus
from singularity.observability import TraceEventType, TraceRecorder
from singularity.oracle.cli import _run_provider_smoke_benchmark, app, workspace_health_summary
from singularity.planner import Planner, TaskStatus, create_or_resume_planner
from singularity.sandbox import WindowsSandboxDoctorReport
from singularity.workspace_state import WorkspaceHealthReport, WorkspaceHealthStatus

runner = CliRunner()


def test_workspace_health_summary_lists_state_categories() -> None:
    health = WorkspaceHealthReport(
        status=WorkspaceHealthStatus.CONFLICTED,
        agent_changes=["app.py"],
        command_side_effects=["generated.txt"],
        external_changes=["README.md"],
        rollback_available=True,
        rollback_conflicts=["app.py"],
        recommended_next_action="re-read changed files before continuing",
    )

    summary = workspace_health_summary(health)

    assert "status: conflicted" in summary
    assert "agent_changes: app.py" in summary
    assert "command_side_effects: generated.txt" in summary
    assert "external_changes: README.md" in summary
    assert "rollback_available: true" in summary
    assert "rollback_conflicts: app.py" in summary
    assert "recommended_next_action: re-read changed files before continuing" in summary


def test_sandbox_doctor_json_emits_machine_readable_report(monkeypatch) -> None:
    class FakeWindowsSandboxBackend:
        def doctor(self) -> WindowsSandboxDoctorReport:
            return WindowsSandboxDoctorReport.ready_for_tests()

    monkeypatch.setattr("singularity.oracle.cli.WindowsSandboxBackend", FakeWindowsSandboxBackend)

    result = runner.invoke(app, ["sandbox", "doctor", "--json"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["schema_version"] == "sandbox.windows.doctor/v2"
    assert payload["available"] is True
    assert payload["enforcement_status"] == "available"
    assert payload["missing_requirements"] == payload["blocking_requirements"]
    assert set(payload["primitives"]) == {
        "restricted_token",
        "job_object",
        "low_integrity",
        "acl",
        "firewall",
        "private_desktop",
    }


def test_create_or_resume_planner_marks_conflicted_workspace_needs_review(tmp_path: Path) -> None:
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Resume task")
    planner.interrupt("pause")
    health = WorkspaceHealthReport(
        status=WorkspaceHealthStatus.CONFLICTED,
        external_changes=["README.md"],
    )

    resumed = create_or_resume_planner(
        workspace_root=tmp_path,
        session_id="session_1",
        task_id="task_2",
        user_goal="Resume task",
        trace=None,
        workspace_health=health,
    )

    assert resumed.state.status == TaskStatus.NEEDS_REVIEW
    assert resumed.evidence.external_changes == ["README.md"]


def test_create_or_resume_planner_starts_new_task_without_resume(tmp_path: Path) -> None:
    planner = create_or_resume_planner(
        workspace_root=tmp_path,
        session_id=None,
        task_id="task_1",
        user_goal="New task",
        trace=None,
        workspace_health=WorkspaceHealthReport(status=WorkspaceHealthStatus.CLEAN),
    )

    assert planner.state.task_id == "task_1"
    assert planner.state.status == TaskStatus.UNDERSTANDING_TASK


def test_cli_runs_through_kernel_bootstrap(monkeypatch, tmp_path: Path) -> None:
    calls: list[tuple[str, object]] = []
    config_dir = tmp_path / ".singularity"
    config_dir.mkdir()
    (config_dir / "config.toml").write_text(
        "max_turns = 4\n[permissions]\nprofile = \"workspace-write\"\n",
        encoding="utf-8",
    )
    extra_dir = tmp_path / "shared"

    class FakeWorkspaceState:
        baseline = None

        def get_workspace_health(self) -> WorkspaceHealthReport:
            return WorkspaceHealthReport(status=WorkspaceHealthStatus.CLEAN)

    class FakeTrace:
        def record(self, event: str, data: dict) -> None:
            calls.append((event, data))

        class Store:
            run_dir = tmp_path / "traces" / "run_1"

        store = Store()

    class FakeGraph:
        trace = FakeTrace()
        workspace_state = FakeWorkspaceState()

    class FakeResult:
        final_answer = "done"
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
            component_health_summary={"planner": "ok"},
            shutdown_summary={"cleanup_status": "completed"},
            recovery_summary={"recovered": False},
            lifecycle_summary={"events": 3},
        )

    class FakeKernel:
        graph = FakeGraph()
        recovery_report = None

        class Context:
            class Identity:
                run_id = "run_1"

            identity = Identity()

        context = Context()

        def run_task(self, goal: str) -> FakeResult:
            calls.append(("run_task", {"goal": goal}))
            return FakeResult()

    class FakeBootstrap:
        def __init__(self, **kwargs) -> None:
            calls.append(("bootstrap_init", kwargs))

        def boot(self, goal: str) -> FakeKernel:
            calls.append(("boot", {"goal": goal}))
            return FakeKernel()

    monkeypatch.chdir(tmp_path)
    monkeypatch.setattr("singularity.oracle.cli.KernelBootstrap", FakeBootstrap)

    result = runner.invoke(
        app,
        [
            "hello",
            "--dry-run",
            "--permission-profile",
            "read-only",
            "--approval-policy",
            "never",
            "--network-access",
            "allowed",
            "--add-dir",
            str(extra_dir),
            "--windows-sandbox",
            "elevated",
        ],
    )

    assert result.exit_code == 0
    assert ("boot", {"goal": "hello"}) in calls
    assert ("run_task", {"goal": "hello"}) in calls
    bootstrap_config = next(
        payload["config"]
        for event, payload in calls
        if event == "bootstrap_init" and isinstance(payload, dict)
    )
    assert bootstrap_config.max_turns == 4
    assert bootstrap_config.dry_run is True
    assert bootstrap_config.permission_profile.value == "read-only"
    assert bootstrap_config.approval_policy.value == "never"
    assert bootstrap_config.network_access.value == "allowed"
    assert bootstrap_config.additional_writable_directories == (extra_dir.resolve(),)
    assert bootstrap_config.windows_sandbox == "elevated"
    assert bootstrap_config.config_sources["max_turns"] == "config:.singularity/config.toml"
    assert bootstrap_config.config_sources["dry_run"] == "cli"
    assert "final report" in result.output
    assert "sg session show" in result.output
    assert "sg continue" in result.output
    assert "sg resume" in result.output


def test_cli_run_accepts_project_root(monkeypatch, tmp_path: Path) -> None:
    calls: list[tuple[str, object]] = []
    project_root = tmp_path / "project"
    cwd = tmp_path / "cwd"
    (project_root / ".singularity").mkdir(parents=True)
    cwd.mkdir()
    (project_root / ".singularity" / "config.toml").write_text("max_turns = 5\n", encoding="utf-8")

    class FakeWorkspaceState:
        baseline = None

        def get_workspace_health(self) -> WorkspaceHealthReport:
            return WorkspaceHealthReport(status=WorkspaceHealthStatus.CLEAN)

    class FakeTrace:
        class Store:
            run_dir = project_root / "traces" / "run_1"

        store = Store()

        def record(self, event: str, data: dict) -> None:
            calls.append((event, data))

    class FakeGraph:
        trace = FakeTrace()
        workspace_state = FakeWorkspaceState()

    class FakeResult:
        final_answer = "done"
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
            component_health_summary={"planner": "ok"},
            shutdown_summary={"cleanup_status": "completed"},
            recovery_summary={"recovered": False},
            lifecycle_summary={"events": 3},
        )

    class FakeKernel:
        graph = FakeGraph()
        recovery_report = None

        class Context:
            class Identity:
                run_id = "run_1"

            identity = Identity()

        context = Context()

        def run_task(self, goal: str) -> FakeResult:
            return FakeResult()

    class FakeBootstrap:
        def __init__(self, **kwargs) -> None:
            calls.append(("bootstrap_init", kwargs))

        def boot(self, goal: str) -> FakeKernel:
            return FakeKernel()

    monkeypatch.chdir(cwd)
    monkeypatch.setattr("singularity.oracle.cli.KernelBootstrap", FakeBootstrap)

    result = runner.invoke(app, ["run", "hello", "--project-root", str(project_root), "--dry-run"])

    assert result.exit_code == 0
    payload = next(item for event, item in calls if event == "bootstrap_init")
    assert payload["project_root"] == project_root.resolve()
    assert payload["config"].project_root == project_root.resolve()
    assert payload["config"].max_turns == 5


def test_index_cli_accepts_explicit_project_root(monkeypatch, tmp_path: Path) -> None:
    project_root = tmp_path / "project"
    cwd = tmp_path / "cwd"
    project_root.mkdir()
    cwd.mkdir()
    (project_root / "app.py").write_text("def main():\n    return 1\n", encoding="utf-8")
    monkeypatch.chdir(cwd)

    result = runner.invoke(app, ["index", "build", "--project-root", str(project_root), "--json"])

    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["file_count"] >= 1
    assert (project_root / ".singularity" / "index.sqlite").exists()
    assert not (cwd / ".singularity" / "index.sqlite").exists()


def test_eval_provider_smoke_uses_kernel_and_independent_smoke(monkeypatch, tmp_path: Path) -> None:
    class FakeTrace:
        class Store:
            run_dir = tmp_path / "trace" / "run_1"

        store = Store()

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
            component_health_summary={"planner": "ok"},
            shutdown_summary={"cleanup_status": "completed"},
            recovery_summary={"recovered": False},
            lifecycle_summary={"events": 3},
        )

    class FakeKernel:
        graph = FakeGraph()

        def __init__(self, project_root: Path) -> None:
            self.project_root = project_root

        def run_task(self, goal: str) -> FakeResult:
            (self.project_root / "quicksort.py").write_text(
                "def quicksort(values):\n"
                "    return sorted(values)\n"
                "if __name__ == '__main__':\n"
                "    assert quicksort([3, 1, 2]) == [1, 2, 3]\n",
                encoding="utf-8",
            )
            return FakeResult()

        def close_resources(self) -> None:
            return None

    class FakeBootstrap:
        def __init__(self, **kwargs) -> None:
            self.project_root = kwargs["project_root"]

        def boot(self, goal: str) -> FakeKernel:
            return FakeKernel(self.project_root)

    monkeypatch.setattr("singularity.oracle.cli.KernelBootstrap", FakeBootstrap)

    result = _run_provider_smoke_benchmark(
        output_dir=tmp_path / "provider",
        run_id="provider_test",
        max_turns=3,
        model=None,
        base_url=None,
    )

    assert result["ok"] is True
    assert result["agent_completed"] is True
    assert result["status"] == "completed"
    assert Path(result["workspace"], "quicksort.py").exists()
    assert result["independent_smoke"]["exit_code"] == 0


def test_eval_provider_smoke_accepts_verified_artifact_when_agent_blocks_late(
    monkeypatch,
    tmp_path: Path,
) -> None:
    class FakeTrace:
        class Store:
            run_dir = tmp_path / "trace" / "run_1"

        store = Store()

    class FakeGraph:
        trace = FakeTrace()

    class BlockedStatus:
        value = "blocked"

    class FakeResult:
        status = BlockedStatus()
        final_report = FinalReport(
            run_id="run_1",
            session_id="session_1",
            task_id="task_1",
            kernel_status="finalized",
            shutdown_reason="blocked",
            diagnostics_count=0,
            cleanup_status="completed",
            recovered_previous_run=False,
            uncertain_transactions=[],
            workspace_lock_status="released",
            component_health_summary={"planner": "ok"},
            shutdown_summary={"cleanup_status": "completed"},
            recovery_summary={"recovered": False},
            lifecycle_summary={"events": 3},
        )

    class FakeKernel:
        graph = FakeGraph()

        def __init__(self, project_root: Path) -> None:
            self.project_root = project_root

        def run_task(self, goal: str) -> FakeResult:
            (self.project_root / "quicksort.py").write_text(
                "def quicksort(values):\n"
                "    return sorted(values)\n"
                "if __name__ == '__main__':\n"
                "    assert quicksort([3, 1, 2]) == [1, 2, 3]\n",
                encoding="utf-8",
            )
            return FakeResult()

        def close_resources(self) -> None:
            return None

    class FakeBootstrap:
        def __init__(self, **kwargs) -> None:
            self.project_root = kwargs["project_root"]

        def boot(self, goal: str) -> FakeKernel:
            return FakeKernel(self.project_root)

    monkeypatch.setattr("singularity.oracle.cli.KernelBootstrap", FakeBootstrap)

    result = _run_provider_smoke_benchmark(
        output_dir=tmp_path / "provider",
        run_id="late_block",
        max_turns=3,
        model=None,
        base_url=None,
    )

    assert result["ok"] is True
    assert result["agent_completed"] is False
    assert result["status"] == "blocked"


@pytest.mark.provider_eval
def test_eval_provider_smoke_real_provider_opt_in(tmp_path: Path) -> None:
    if os.getenv("SINGULARITY_RUN_PROVIDER_EVAL") != "1":
        pytest.skip("set SINGULARITY_RUN_PROVIDER_EVAL=1 to run provider evaluation")
    required = ["SINGULARITY_API_KEY", "SINGULARITY_MODEL", "SINGULARITY_BASE_URL"]
    missing = [name for name in required if not os.getenv(name)]
    if missing:
        pytest.skip(f"missing provider environment: {', '.join(missing)}")

    result = _run_provider_smoke_benchmark(
        output_dir=tmp_path / "provider",
        run_id="provider",
        max_turns=12,
        model=None,
        base_url=None,
    )

    payload = json.dumps(result, ensure_ascii=False, default=str)
    api_key = os.environ["SINGULARITY_API_KEY"]
    assert result["ok"] is True
    assert result["independent_smoke"]["exit_code"] == 0
    assert "SINGULARITY_API_KEY" not in payload
    leaked_api_key = api_key in payload
    api_key_fingerprint = sha256(api_key.encode()).hexdigest()
    assert (
        not leaked_api_key
    ), f"provider smoke payload leaked api key sha256={api_key_fingerprint}"


def test_cli_converts_kernel_cancellation_to_exit(monkeypatch, tmp_path: Path) -> None:
    final_report = FinalReport(
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        kernel_status="finalized",
        shutdown_reason="keyboard_interrupt",
        diagnostics_count=0,
        cleanup_status="completed",
        recovered_previous_run=False,
        uncertain_transactions=[],
        workspace_lock_status="released",
        shutdown_summary={"reason": "keyboard_interrupt", "cleanup_status": "completed"},
    )

    class FakeKernel:
        class FakeWorkspaceState:
            baseline = None

            def get_workspace_health(self) -> WorkspaceHealthReport:
                return WorkspaceHealthReport(status=WorkspaceHealthStatus.CLEAN)

        class FakeTrace:
            class Store:
                run_dir = tmp_path / "traces" / "run_1"

            store = Store()

            def record(self, event: str, data: dict) -> None:
                pass

        class FakeGraph:
            pass

        graph = FakeGraph()
        graph.trace = FakeTrace()
        graph.workspace_state = FakeWorkspaceState()

        class Context:
            class Identity:
                run_id = "run_1"

            identity = Identity()

        context = Context()
        recovery_report = None

        def run_task(self, goal: str):
            raise CancellationError("Ctrl+C", code="keyboard_interrupt")

        def final_report(self):
            return final_report

    class FakeBootstrap:
        def __init__(self, **kwargs) -> None:
            pass

        def boot(self, goal: str):
            return FakeKernel()

    monkeypatch.chdir(tmp_path)
    monkeypatch.setattr("singularity.oracle.cli.KernelBootstrap", FakeBootstrap)

    result = runner.invoke(app, ["main", "hello", "--dry-run"])

    assert result.exit_code == 1
    assert "cancelled" in result.output
    assert "final report" in result.output
    assert "keyboard_interrupt" in result.output


def test_cli_eval_task_validate_and_list_filter_tags(tmp_path: Path) -> None:
    task_set = tmp_path / "golden.json"
    task = BenchmarkTask(
        task_id="task.cli",
        version="v1",
        title="CLI task",
        input_prompt="Run the evaluation CLI.",
        workspace_snapshot=WorkspaceSnapshot(
            kind=WorkspaceSnapshotKind.GIT_REF,
            git_ref="HEAD",
        ),
        expected_outcomes=[
            ExpectedOutcome(kind=ExpectedOutcomeKind.HEURISTIC, weight=1.0)
        ],
        tags=["easy", "tool-heavy"],
    )
    task_set.write_text(GoldenTaskStore.to_json_document([task]), encoding="utf-8")

    validate = runner.invoke(app, ["eval", "task", "validate", str(task_set), "--json"])
    listed = runner.invoke(
        app,
        ["eval", "task", "list", str(task_set), "--tag", "tool-heavy", "--json"],
    )

    assert validate.exit_code == 0
    assert '"task_count": 1' in validate.output
    assert listed.exit_code == 0
    assert "task.cli" in listed.output
    assert "benchmark" not in get_command(app).commands


def test_cli_plugin_lifecycle_json_does_not_import_disabled_plugin(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.chdir(tmp_path)
    plugin_dir = tmp_path / ".singularity" / "plugins" / "cli_plugin"
    plugin_dir.mkdir(parents=True)
    sentinel = tmp_path / "imported.txt"
    _write_plugin_manifest(plugin_dir)
    (plugin_dir / "plugin.py").write_text(
        f"from pathlib import Path\nPath({str(sentinel)!r}).write_text('imported')\n"
        "def register(host):\n"
        "    pass\n",
        encoding="utf-8",
    )

    listed = runner.invoke(app, ["plugin", "list", "--json"])
    inspected = runner.invoke(app, ["plugin", "inspect", "cli_plugin", "--json"])
    checked = runner.invoke(app, ["plugin", "check", "cli_plugin", "--json"])
    enabled = runner.invoke(app, ["plugin", "enable", "cli_plugin", "--json"])
    disabled = runner.invoke(app, ["plugin", "disable", "cli_plugin", "--json"])

    assert listed.exit_code == 0
    assert inspected.exit_code == 0
    assert checked.exit_code == 0
    assert enabled.exit_code == 0
    assert disabled.exit_code == 0
    assert json.loads(listed.output)["plugins"][0]["id"] == "cli_plugin"
    assert json.loads(inspected.output)["manifest"]["id"] == "cli_plugin"
    assert json.loads(enabled.output)["ok"] is True
    assert json.loads(disabled.output)["status"]["enabled"] is False
    assert not sentinel.exists()


def _write_plugin_manifest(plugin_dir: Path) -> None:
    (plugin_dir / "plugin.toml").write_text(
        """
id = "cli_plugin"
name = "CLI Plugin"
version = "0.1.0"
api_version = "1"
entrypoint = "plugin.py:register"
type = "tool"
capabilities = ["echo"]
permissions = ["read_workspace"]

[activation]
mode = "manual"

[compatibility]
min_python = "3.11"

[config_schema]
type = "object"
additionalProperties = false
""".strip()
        + "\n",
        encoding="utf-8",
    )


def _cli_eval_task(task_id: str = "task.cli.suite") -> BenchmarkTask:
    return BenchmarkTask(
        task_id=task_id,
        version="v1",
        title="CLI evaluation task",
        input_prompt="Run the evaluation CLI.",
        workspace_snapshot=WorkspaceSnapshot(
            kind=WorkspaceSnapshotKind.GIT_REF,
            git_ref="HEAD",
        ),
        expected_outcomes=[
            ExpectedOutcome(
                kind=ExpectedOutcomeKind.HEURISTIC,
                weight=1.0,
                heuristic="custom",
                metadata={"score": 0.7},
            )
        ],
        tags=["easy", "tool-heavy"],
    )


def _write_cli_task_set(path: Path) -> None:
    path.write_text(GoldenTaskStore.to_json_document([_cli_eval_task()]), encoding="utf-8")


def _write_cli_trace(root: Path) -> Path:
    trace = TraceRecorder.create(root, run_id="run_cli", session_id="session_cli")
    trace.emit(
        TraceEventType.MODEL_RESPONSE_RECEIVED,
        component="model",
        summary="Model response received.",
        payload={"usage": {"input_tokens": 10, "output_tokens": 5, "cost_estimate": 0.01}},
    )
    trace.emit(
        TraceEventType.VERIFICATION_CHECK_COMPLETED,
        component="verification",
        summary="check passed",
        payload={"status": "passed"},
    )
    return trace.store.run_dir


def test_cli_eval_suite_run_writes_report_without_singularity_state(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.chdir(tmp_path)
    task_set = tmp_path / "golden.json"
    output_dir = tmp_path / "evals"
    _write_cli_task_set(task_set)

    result = runner.invoke(
        app,
        [
            "eval",
            "suite",
            "run",
            str(task_set),
            "--output-dir",
            str(output_dir),
            "--run-id",
            "suite_cli",
            "--json",
        ],
    )

    assert result.exit_code == 0
    assert (output_dir / "suite_cli" / "report.json").exists()
    assert (output_dir / "suite_cli" / "report.md").exists()
    assert not (tmp_path / ".singularity").exists()
    payload = json.loads((output_dir / "suite_cli" / "report.json").read_text(encoding="utf-8"))
    assert payload["run_id"] == "suite_cli"
    assert payload["profile_reports"][0]["task_results"][0]["agent_config_overrides"]["model"] == "default"


def test_cli_eval_trace_replay_outputs_deterministic_hash(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.chdir(tmp_path)
    trace_run_dir = _write_cli_trace(tmp_path)

    result = runner.invoke(app, ["eval", "trace", "replay", str(trace_run_dir), "--json"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["deterministic"] is True
    assert payload["trace_input_digest"]
    assert payload["result_hash"]


def test_cli_eval_targeted_replay_writes_repair_replay_artifacts(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.chdir(tmp_path)
    output_dir = tmp_path / "targeted"

    result = runner.invoke(
        app,
        [
            "eval",
            "targeted-replay",
            "--output-dir",
            str(output_dir),
            "--json",
        ],
    )

    payload = json.loads(result.output)
    assert payload["entered_agent_loop"] is True
    blocked_reason = str(payload.get("repair_contract_summary", {}).get("blocked_reason") or "")
    if "sandbox backend unavailable" in blocked_reason:
        assert result.exit_code == 1, result.output
        assert payload["status"] == "blocked"
    else:
        assert result.exit_code == 0, result.output
        assert payload["status"] == "completed"
    assert payload["repairing_failures_seen"] is True
    assert "model_visible_objects" not in payload
    assert "evaluator_internal_objects" not in payload
    assert payload["report_paths"]["json"] == str(output_dir / "targeted_replay_result.json")
    assert payload["report_paths"]["markdown"] == str(output_dir / "targeted_replay_result.md")
    assert (output_dir / "targeted_replay_result.json").exists()
    assert (output_dir / "targeted_replay_result.md").exists()


def test_cli_eval_ab_and_regression_run_persist_reports(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.chdir(tmp_path)
    task_set = tmp_path / "golden.json"
    output_dir = tmp_path / "evals"
    trace_run_dir = _write_cli_trace(tmp_path)
    _write_cli_task_set(task_set)
    baseline = json.dumps(
        {
            "name": "baseline",
            "model": "gpt-a",
            "prompt_profile": "default",
            "memory_enabled": True,
            "allowed_tools": [],
            "tool_policy": "read_write",
        }
    )
    candidate = json.dumps(
        {
            "name": "candidate",
            "model": "gpt-b",
            "prompt_profile": "compact",
            "memory_enabled": False,
            "allowed_tools": ["read_file"],
            "tool_policy": "read_only",
        }
    )

    ab_result = runner.invoke(
        app,
        [
            "eval",
            "ab",
            "run",
            str(task_set),
            "--baseline-profile-json",
            baseline,
            "--candidate-profile-json",
            candidate,
            "--trace-run-dir",
            str(trace_run_dir),
            "--output-dir",
            str(output_dir),
            "--run-id",
            "ab_cli",
            "--json",
        ],
    )
    regression_result = runner.invoke(
        app,
        [
            "eval",
            "regression",
            "run",
            str(task_set),
            "--baseline-profile-json",
            baseline,
            "--candidate-profile-json",
            candidate,
            "--trace-run-dir",
            str(trace_run_dir),
            "--output-dir",
            str(output_dir),
            "--run-id",
            "reg_cli",
            "--json",
        ],
    )

    assert ab_result.exit_code == 0
    assert regression_result.exit_code == 0
    assert (output_dir / "ab_cli" / "report.json").exists()
    assert (output_dir / "reg_cli" / "report.json").exists()
    assert (output_dir / "reg_cli" / "regression.json").exists()
    regression_payload = json.loads((output_dir / "reg_cli" / "regression.json").read_text(encoding="utf-8"))
    assert regression_payload["baseline_profile"] == "baseline"
    assert regression_payload["candidate_profile"] == "candidate"


def test_cli_eval_report_show_reads_json_and_markdown(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.chdir(tmp_path)
    task_set = tmp_path / "golden.json"
    output_dir = tmp_path / "evals"
    _write_cli_task_set(task_set)
    run = runner.invoke(
        app,
        [
            "eval",
            "suite",
            "run",
            str(task_set),
            "--output-dir",
            str(output_dir),
            "--run-id",
            "show_cli",
        ],
    )
    assert run.exit_code == 0

    shown_json = runner.invoke(
        app,
        ["eval", "report", "show", str(output_dir / "show_cli" / "report.json"), "--json"],
    )
    shown_md = runner.invoke(
        app,
        ["eval", "report", "show", str(output_dir / "show_cli" / "report.md")],
    )

    assert shown_json.exit_code == 0
    assert '"run_id": "show_cli"' in shown_json.output
    assert shown_md.exit_code == 0
    assert "# Evaluation Report `show_cli`" in shown_md.output


def test_cli_eval_private_uses_private_benchmark_adapter(tmp_path: Path, monkeypatch) -> None:
    task_set = tmp_path / "private.json"
    task = BenchmarkTask(
        task_id="private.cli",
        version="v1",
        title="Private CLI eval",
        input={
            "prompt": "Fix app.py.",
            "metadata": {"allowed_paths": ["app.py"]},
        },
        workspace_snapshot=WorkspaceSnapshot(
            kind=WorkspaceSnapshotKind.INLINE_FILES,
            inline_files={"app.py": "def answer():\n    return 0\n"},
        ),
        expected_outcomes=[
            ExpectedOutcome(kind=ExpectedOutcomeKind.TEST, weight=1.0, command="python -m pytest tests/test_app.py")
        ],
        tags=["easy"],
    )
    task_set.write_text(GoldenTaskStore.to_json_document([task]), encoding="utf-8")
    seen = {}

    class FakeRunner:
        def __init__(self, **kwargs) -> None:
            seen["kwargs"] = kwargs

        def run(self, manifest):
            seen["task_id"] = manifest.tasks[0].task_id
            seen["verification_command"] = manifest.tasks[0].verification_command
            return {
                "schema_version": "evaluation.result/v1",
                "run_id": "private_cli",
                "summary": {"evaluation_passed_count": 1, "task_count": 1},
                "tasks": [],
            }

    monkeypatch.setattr("singularity.oracle.cli.EvaluationRunner", FakeRunner)

    result = runner.invoke(
        app,
        [
            "eval",
            "private",
            str(task_set),
            "--run-id",
            "private_cli",
            "--project-root",
            str(tmp_path),
            "--json",
        ],
    )

    assert result.exit_code == 0, result.output
    assert json.loads(result.output)["run_id"] == "private_cli"
    assert seen["task_id"] == "private.cli"
    assert seen["verification_command"] == "python -m pytest tests/test_app.py"
    assert seen["kwargs"]["env_root"] == tmp_path.resolve()

    # Regression guard: the retired evaluation subcommand is intentionally absent.
    removed = runner.invoke(app, ["eval", "live", "private", str(task_set)])
    assert removed.exit_code != 0
    assert "No such command" in removed.output
    assert "benchmark" not in get_command(app).commands


def test_cli_eval_run_rejects_unsupported_manifest_schema(
    tmp_path: Path,
    monkeypatch,
) -> None:
    task_set = tmp_path / "old-entry.json"
    old_schema = "evaluation." + "live" + "_agent_task_set/v1"
    task_set.write_text(
        json.dumps(
            {
                "schema_version": old_schema,
                "tasks": [
                    {
                        "task_id": "old.cli",
                        "workspace": {"type": "fixture", "files": {"README.md": "fixture\n"}},
                        "user_task": "Say done.",
                        "allowed_paths": ["."],
                        "verification_command": "python -c \"print('ok')\"",
                        "success": {"type": "verification_exit_code", "exit_code": 0},
                    }
                ],
            }
        ),
        encoding="utf-8",
    )
    seen = {"ran": False}

    class FakeRunner:
        def __init__(self, **kwargs) -> None:
            seen["kwargs"] = kwargs

        def run(self, manifest):
            seen["ran"] = True
            return {
                "schema_version": "evaluation.result/v1",
                "run_id": "canonical",
                "summary": {"evaluation_passed_count": 1, "task_count": 1},
                "tasks": [],
            }

    monkeypatch.setattr("singularity.oracle.cli.EvaluationRunner", FakeRunner)

    result = runner.invoke(app, ["eval", "run", str(task_set), "--json"])

    assert result.exit_code != 0
    assert isinstance(result.exception, ValueError)
    assert "Unsupported evaluation schema_version" in str(result.exception)
    assert seen["ran"] is False


def test_evaluation_task_set_runners_share_cli_runner_body() -> None:
    import inspect

    from singularity.oracle import cli

    source = inspect.getsource(cli)
    assert "def _run_loaded_evaluation_task_set" in source
    assert source.count("EvaluationRunner(") == 1

