from __future__ import annotations

import json
import tomllib
from pathlib import Path

import pytest

from scripts.verify_gate_common import capability_timing_from_result

yaml = pytest.importorskip("yaml")

pytestmark = pytest.mark.regression

_REQUIRED_RUFF_RULES = {"F", "I", "B", "SIM", "UP", "RUF"}
_REQUIRED_CI_COMMANDS = (
    "python -m ruff check .",
    "python -m mypy",
    "python -m compileall src scripts",
    "python scripts/verify_runtime_docs.py",
    'python -m pytest tests --basetemp work/pytest-tmp -m "not provider_eval and not external and not slow"',
)
_REQUIRED_LOCAL_GATE_SCRIPTS = (
    "scripts/verify_fast.py",
    "scripts/verify_stage.py",
    "scripts/verify_capability.py",
)


def _project_configuration() -> dict[str, object]:
    return tomllib.loads(Path("pyproject.toml").read_text(encoding="utf-8"))


def _workflow_configuration() -> dict[str, object]:
    payload = yaml.safe_load(Path(".github/workflows/ci.yml").read_text(encoding="utf-8"))
    assert isinstance(payload, dict)
    return payload


def test_static_quality_gate_configuration() -> None:
    config = _project_configuration()
    tool_config = config["tool"]
    assert isinstance(tool_config, dict)

    ruff_config = tool_config["ruff"]
    mypy_config = tool_config["mypy"]
    assert isinstance(ruff_config, dict)
    assert isinstance(mypy_config, dict)

    lint_config = ruff_config["lint"]
    assert isinstance(lint_config, dict)
    assert set(lint_config["select"]) >= _REQUIRED_RUFF_RULES
    assert mypy_config["check_untyped_defs"] is True
    assert mypy_config.get("follow_imports") != "skip"


def test_ci_workflow_enforces_cross_platform_quality_gates() -> None:
    workflow = _workflow_configuration()
    jobs = workflow["jobs"]
    assert isinstance(jobs, dict)
    quality = jobs["quality"]
    assert isinstance(quality, dict)
    strategy = quality["strategy"]
    assert isinstance(strategy, dict)
    matrix = strategy["matrix"]
    assert isinstance(matrix, dict)
    assert set(matrix["os"]) == {"ubuntu-latest", "windows-latest"}
    assert set(matrix["python-version"]) == {"3.11", "3.12", "3.13", "3.14"}

    steps = quality["steps"]
    assert isinstance(steps, list)
    commands = [str(step.get("run") or "") for step in steps if isinstance(step, dict)]
    assert any("uv sync --locked --extra eval --group dev" in command for command in commands)
    for required in _REQUIRED_CI_COMMANDS:
        assert any(required in command for command in commands)



def test_local_tiered_verification_gate_scripts_exist() -> None:
    for script in _REQUIRED_LOCAL_GATE_SCRIPTS:
        path = Path(script)
        assert path.exists(), f"missing local verification gate: {script}"
        text = path.read_text(encoding="utf-8")
        assert "duration_seconds" in text
        assert "timing" in text
        assert "json" in text.lower()


def test_capability_gate_defaults_to_public_task_and_keeps_internal_regression_legacy() -> None:
    capability = Path("scripts/verify_capability.py").read_text(encoding="utf-8")
    assert 'DEFAULT_MANIFEST = "docs/evaluation/public-representative-task.json"' in capability
    assert "capability-regression-tasks.json" not in capability

    assert Path("docs/evaluation/public-representative-task.json").exists()
    assert Path("docs/evaluation/legacy/internal-smoke-regression-tasks.json").exists()
    assert not Path("docs/evaluation/capability-regression-tasks.json").exists()


def test_gate_scripts_expose_structured_timing_contract() -> None:
    common = Path("scripts/verify_gate_common.py").read_text(encoding="utf-8")
    for required in [
        "total_wall_time_seconds",
        "ruff_time_seconds",
        "mypy_time_seconds",
        "compileall_time_seconds",
        "pytest_time_seconds",
        "runtime_docs_time_seconds",
        "capability_eval_time_seconds",
        "provider_time_seconds",
        "sandbox_time_seconds",
        "verification_time_seconds",
        "context_retrieval_compaction_time_seconds",
        "selected_tests_count",
        "skipped_tests_count",
        "fallback_reason",
    ]:
        assert required in common

    fast = Path("scripts/verify_fast.py").read_text(encoding="utf-8")
    assert "stage_gate_recommended" in fast
    assert "selected_tests" in fast


def test_capability_timing_reads_task_result_timing(tmp_path: Path) -> None:
    result_path = tmp_path / "result.json"
    result_path.write_text(
        json.dumps(
            {
                "tasks": [
                    {
                        "timing": {
                            "provider_time_seconds": 1.25,
                            "sandbox_time_seconds": 0.5,
                            "verification_time_seconds": 2.0,
                            "context_retrieval_compaction_time_seconds": 0.75,
                        },
                        "capability_summary": {
                            "context_package_rebuild_count": 3,
                            "context_compaction": {"requested": 1},
                        },
                    }
                ]
            }
        ),
        encoding="utf-8",
    )

    assert capability_timing_from_result(result_path) == {
        "provider_time_seconds": 1.25,
        "sandbox_time_seconds": 0.5,
        "verification_time_seconds": 2.0,
        "context_retrieval_compaction_time_seconds": 0.75,
    }


def test_local_gate_pytest_commands_override_default_evaluation_exclusion() -> None:
    fast = Path("scripts/verify_fast.py").read_text(encoding="utf-8")
    stage = Path("scripts/verify_stage.py").read_text(encoding="utf-8")

    assert "not provider_eval and not slow and not external" in fast
    assert "evaluation and not provider_eval and not slow and not external" in stage


def test_quality_matrix_remains_two_os_by_four_python_versions() -> None:
    workflow = _workflow_configuration()
    jobs = workflow["jobs"]
    quality = jobs["quality"]
    strategy = quality["strategy"]
    matrix = strategy["matrix"]

    combinations = [
        (os_name, python_version)
        for os_name in matrix["os"]
        for python_version in matrix["python-version"]
    ]

    assert len(combinations) == 8
    assert set(matrix["os"]) == {"ubuntu-latest", "windows-latest"}
    assert set(matrix["python-version"]) == {"3.11", "3.12", "3.13", "3.14"}


def test_provider_validation_is_explicitly_gated_without_fake_fallback() -> None:
    workflow = _workflow_configuration()
    jobs = workflow["jobs"]
    assert isinstance(jobs, dict)
    provider_job = jobs["provider-evaluation"]
    assert isinstance(provider_job, dict)
    job_condition = str(provider_job["if"])
    assert "github.event_name == 'schedule'" in job_condition
    assert "github.event_name == 'workflow_dispatch'" in job_condition
    job_env = provider_job.get("env") or {}
    assert isinstance(job_env, dict)
    assert not {"SINGULARITY_API_KEY", "SINGULARITY_MODEL", "SINGULARITY_BASE_URL"} & set(job_env)

    steps = provider_job["steps"]
    assert isinstance(steps, list)
    steps_by_id = {step.get("id"): step for step in steps if isinstance(step, dict) and step.get("id")}
    guard = steps_by_id["provider-config"]
    guard_env = guard.get("env") or {}
    assert isinstance(guard_env, dict)
    assert set(guard_env) == {"SINGULARITY_API_KEY", "SINGULARITY_MODEL", "SINGULARITY_BASE_URL"}
    assert "GITHUB_OUTPUT" in str(guard.get("run") or "")

    real_steps = [
        step
        for step in steps
        if isinstance(step, dict) and step.get("name") in {"Run real-provider tests", "Run real evaluation benchmark"}
    ]
    assert len(real_steps) == 2
    for step in real_steps:
        assert "steps.provider-config.outputs.configured == 'true'" in str(step.get("if") or "")
        step_env = step.get("env") or {}
        assert isinstance(step_env, dict)
        assert {"SINGULARITY_API_KEY", "SINGULARITY_MODEL", "SINGULARITY_BASE_URL"} <= set(step_env)
    commands = "\n".join(str(step.get("run") or "") for step in steps if isinstance(step, dict))
    assert "provider_eval skipped" in commands
    assert "python -m pytest tests -m provider_eval" in commands
    assert "python scripts/verify_capability.py --force" in commands
    assert "docs/evaluation/capability-regression-tasks.json" not in commands
    lowered = commands.lower()
    assert "fake provider" not in lowered
    assert "mock provider" not in lowered
    assert "scripted provider" not in lowered
