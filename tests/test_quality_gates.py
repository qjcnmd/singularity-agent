from __future__ import annotations

import json
import tomllib
from pathlib import Path

import pytest

from scripts.verify_gate_common import (
    capability_latency_attribution_from_result,
    capability_metrics_from_result,
    capability_repeated_timing_compare,
    capability_review_from_result,
    capability_sla_diagnostics,
    capability_sla_from_result,
    capability_timing_from_result,
    capability_turns_from_result,
)

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


def test_capability_gate_defaults_to_public_task_only() -> None:
    capability = Path("scripts/verify_capability.py").read_text(encoding="utf-8")
    assert 'DEFAULT_MANIFEST = "docs/evaluation/public-representative-task.json"' in capability
    old_regression_manifest = "capability-" + "regression-tasks.json"
    legacy_internal_smoke = "legacy/internal-" + "smoke-regression-tasks.json"

    assert Path("docs/evaluation/public-representative-task.json").exists()
    assert old_regression_manifest not in capability
    assert not Path("docs/evaluation", legacy_internal_smoke).exists()
    assert not Path("docs/evaluation", old_regression_manifest).exists()
    assert not Path("docs/evaluation", "capability-" + "minimal-tasks.json").exists()
    assert not Path("docs/evaluation", "capability-fix-" + "math-test-only.json").exists()
    assert not Path("docs/evaluation", "evaluation-baseline-" + "example.json").exists()
    manifest = json.loads(Path("docs/evaluation/public-representative-task.json").read_text(encoding="utf-8"))
    assert [task["task_id"] for task in manifest["tasks"]] == ["sqlfluff__sqlfluff-2419"]


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
                            "workspace_materialization_time_seconds": 4.0,
                            "repo_fetch_time_seconds": None,
                        },
                        "capability_summary": {
                            "context_package_rebuild_count": 3,
                            "context_compaction": {"requested": 1},
                            "timing_diagnostics": {
                                "repo_fetch_time_seconds": {
                                    "status": "not_applicable",
                                    "source": "evaluation_runner",
                                    "reason": "clone path",
                                }
                            },
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
        "workspace_materialization_time_seconds": 4.0,
        "repo_fetch_time_seconds": None,
        "timing_diagnostics": {
            "repo_fetch_time_seconds": {
                "status": "not_applicable",
                "source": "evaluation_runner",
                "reason": "clone path",
            }
        },
    }


def test_capability_metrics_reads_task_scorecard(tmp_path: Path) -> None:
    result_path = tmp_path / "result.json"
    result_path.write_text(
        json.dumps(
            {
                "summary": {
                    "resolved_count": 1,
                    "resolved_rate": 1.0,
                    "total_cost_estimate": 0.42,
                    "cost_per_resolved": 0.42,
                    "average_tool_success_rate": 0.75,
                },
                "tasks": [
                    {
                        "evaluation_metrics": {
                            "schema_version": "evaluation.metrics/v1",
                            "cost": {
                                "cost_estimate": 0.42,
                                "cost_source": "pricing_table",
                                "pricing_status": "priced",
                            },
                            "efficiency": {
                                "wall_time_seconds": 3.0,
                                "provider_time_seconds": 1.0,
                                "verification_time_seconds": 2.0,
                            },
                        }
                    }
                ],
            }
        ),
        encoding="utf-8",
    )

    assert capability_metrics_from_result(result_path) == {
        "resolved_count": 1,
        "resolved_rate": 1.0,
        "total_cost_estimate": 0.42,
        "cost_per_resolved": 0.42,
        "average_tool_success_rate": 0.75,
        "cost_sources": {"pricing_table": 1},
        "pricing_statuses": {"priced": 1},
        "task_metrics_count": 1,
    }

    payload = json.loads(result_path.read_text(encoding="utf-8"))
    payload["summary"]["average_tool_success_rate"] = None
    result_path.write_text(json.dumps(payload), encoding="utf-8")
    assert capability_metrics_from_result(result_path)["average_tool_success_rate"] is None


def test_capability_sla_reads_task_result_diagnostics(tmp_path: Path) -> None:
    result_path = tmp_path / "result.json"
    result_path.write_text(
        json.dumps(
            {
                "summary": {
                    "capability_sla": {
                        "schema_version": "evaluation.capability_sla_summary/v1",
                        "status": "over_sla",
                        "blocking": False,
                        "violations": {"wall": 1},
                        "task_count": 1,
                    }
                },
                "tasks": [
                    {
                        "capability_sla": {
                            "schema_version": "evaluation.capability_sla/v1",
                            "status": "over_sla",
                            "blocking": False,
                            "violations": ["wall"],
                            "items": {
                                "wall": {
                                    "actual_seconds": 305.543,
                                    "target_seconds": 300.0,
                                    "status": "over_sla",
                                    "delta_seconds": 5.543,
                                    "blocking": False,
                                },
                                "local_fallback": {
                                    "actual_count": 0,
                                    "target_count": 0,
                                    "status": "within_sla",
                                    "blocking": False,
                                },
                                "visibility_audit": {
                                    "passed": True,
                                    "status": "passed",
                                    "blocking": False,
                                },
                            },
                        }
                    }
                ],
            }
        ),
        encoding="utf-8",
    )

    assert capability_sla_from_result(result_path) == {
        "schema_version": "evaluation.capability_sla_summary/v1",
        "status": "over_sla",
        "blocking": False,
        "violations": {"wall": 1},
        "task_count": 1,
        "items": {
            "wall": {
                "actual_seconds": 305.543,
                "target_seconds": 300.0,
                "status": "over_sla",
                "delta_seconds": 5.543,
                "blocking": False,
            },
            "local_fallback": {
                "actual_count": 0,
                "target_count": 0,
                "status": "within_sla",
                "blocking": False,
            },
            "visibility_audit": {
                "passed": True,
                "status": "passed",
                "blocking": False,
            },
        },
    }


def test_capability_gate_reads_turn_and_review_diagnostics(tmp_path: Path) -> None:
    result_path = tmp_path / "result.json"
    result_path.write_text(
        json.dumps(
            {
                "tasks": [
                    {
                        "task_id": "task_1",
                        "capability_summary": {
                            "turn_diagnostics": [
                                {
                                    "turn": 1,
                                    "phase_id": "applying_changes",
                                    "purpose": "plan_next_action",
                                    "provider_duration_seconds": 2.5,
                                    "tool_calls": [{"tool_name": "edit_apply", "status": "ok"}],
                                    "review_events": [
                                        {
                                            "stage": "pre_edit",
                                            "duration_seconds": 0.8,
                                            "critic_duration_seconds": 0.6,
                                            "model_critic_status": "ok",
                                            "output_mode": "structured_output",
                                            "schema_validation_passed": True,
                                            "retry_count": 0,
                                            "retry_reason": "none",
                                            "fallback_reason": "",
                                            "critic_source_status": "ok",
                                            "critic_reuse_skip_reason": "stage_not_reusable",
                                        }
                                    ],
                                }
                            ],
                            "provider_latency_by_review_stage": {
                                "pre_edit": {
                                    "call_count": 1,
                                    "failed_call_count": 0,
                                    "total_seconds": 0.6,
                                    "max_seconds": 0.6,
                                }
                            },
                            "timing": {
                                "edit_apply_review_time_seconds": 0.8,
                                "edit_apply_critic_time_seconds": 0.6,
                            },
                        },
                    }
                ]
            }
        ),
        encoding="utf-8",
    )

    assert capability_turns_from_result(result_path) == {
        "turn_count": 1,
        "provider_time_seconds": 2.5,
        "tool_call_count": 1,
        "review_event_count": 1,
        "slowest_turns": [
            {
                "turn": 1,
                "phase_id": "applying_changes",
                "purpose": "plan_next_action",
                "provider_duration_seconds": 2.5,
                "tool_call_count": 1,
                "review_event_count": 1,
            }
        ],
    }
    assert capability_review_from_result(result_path) == {
        "edit_apply_review_time_seconds": 0.8,
        "edit_apply_critic_time_seconds": 0.6,
        "review_event_count": 1,
        "critic_reused_count": 0,
        "critic_skipped_count": 0,
        "review_events": [
            {
                "task_id": "task_1",
                "turn": 1,
                "stage": "pre_edit",
                "duration_seconds": 0.8,
                "critic_duration_seconds": 0.6,
                "model_critic_status": "ok",
                "output_mode": "structured_output",
                "schema_validation_passed": True,
                "retry_count": 0,
                "retry_reason": "none",
                "fallback_reason": "",
                "critic_reused": False,
                "critic_skipped_reason": "",
                "critic_reuse_skip_reason": "stage_not_reusable",
                "critic_source_status": "ok",
            }
        ],
        "provider_latency_by_review_stage": {
            "pre_edit": {
                "call_count": 1,
                "failed_call_count": 0,
                "total_seconds": 0.6,
                "max_seconds": 0.6,
            }
        },
    }


def test_capability_gate_reads_latency_attribution_and_critical_path(tmp_path: Path) -> None:
    result_path = tmp_path / "result.json"
    result_path.write_text(
        json.dumps(
            {
                "tasks": [
                    {
                        "task_id": "task_1",
                        "capability_summary": {
                            "latency_attribution": {
                                "schema_version": "evaluation.latency_attribution/v1",
                                "items": {
                                    "provider_latency": {
                                        "component": "provider_latency",
                                        "actual_seconds": 12.5,
                                        "source": "trace.model_turns",
                                        "kind": "model_provider",
                                        "critical_path": True,
                                        "status": "measured",
                                        "notes": "",
                                    },
                                    "unattributed_time": {
                                        "component": "unattributed_time",
                                        "actual_seconds": 1.2,
                                        "source": "capability_summary.unattributed_time_seconds",
                                        "kind": "timing_gap",
                                        "critical_path": False,
                                        "status": "diagnostic",
                                        "notes": "diagnostic only; does not affect evaluation_passed",
                                    },
                                },
                            },
                            "critical_path_breakdown": [
                                {
                                    "component": "provider_latency",
                                    "actual_seconds": 12.5,
                                    "source": "trace.model_turns",
                                    "kind": "model_provider",
                                    "critical_path": True,
                                    "status": "measured",
                                    "notes": "",
                                }
                            ],
                        },
                    }
                ]
            }
        ),
        encoding="utf-8",
    )

    attribution = capability_latency_attribution_from_result(result_path)

    assert attribution["schema_version"] == "evaluation.latency_attribution_summary/v1"
    assert attribution["task_count"] == 1
    assert attribution["items"]["provider_latency"]["actual_seconds"] == 12.5
    assert attribution["items"]["unattributed_time"]["status"] == "diagnostic"
    assert attribution["critical_path_breakdown"] == [
        {
            "task_id": "task_1",
            "component": "provider_latency",
            "actual_seconds": 12.5,
            "source": "trace.model_turns",
            "kind": "model_provider",
            "critical_path": True,
            "status": "measured",
            "notes": "",
        }
    ]
    capability_script = Path("scripts/verify_capability.py").read_text(encoding="utf-8")
    assert '"latency_attribution": latency_attribution' in capability_script
    assert '"critical_path_breakdown": latency_attribution.get("critical_path_breakdown", [])' in capability_script
    assert '"8_11_vs_current_timing": phase_8_11_vs_current_timing' in capability_script


def test_capability_repeated_timing_compare_reports_min_median_current(tmp_path: Path) -> None:
    output_root = tmp_path / "work" / "evaluations"
    timings = [
        ("run_a", 300.0, 200.0, 40.0, 50.0, 20.0, 5.0, 10.0),
        ("run_b", 330.0, 220.0, 30.0, 70.0, 22.0, 6.0, 12.0),
        ("run_c", 360.0, 240.0, 35.0, 60.0, 24.0, 7.0, 14.0),
    ]
    for run_id, wall, agent_loop, dependency, sandbox, provider, verification, unattributed in timings:
        run_dir = output_root / run_id
        run_dir.mkdir(parents=True)
        (run_dir / "result.json").write_text(
            json.dumps(
                {
                    "tasks": [
                        {
                            "task_id": "task.public",
                            "reproducible_environment": {
                                "workspace": {
                                    "type": "repo",
                                    "start_commit": "abc123",
                                }
                            },
                            "timing": {
                                "wall_time_seconds": wall,
                                "dependency_setup_time_seconds": dependency,
                                "sandbox_time_seconds": sandbox,
                                "provider_time_seconds": provider,
                                "verification_time_seconds": verification,
                            },
                            "capability_summary": {
                                "wall_phases": {
                                    "agent_loop_time_seconds": agent_loop,
                                },
                                "unattributed_time_seconds": unattributed,
                                "latency_attribution": {
                                    "schema_version": "evaluation.latency_attribution/v1",
                                    "items": {
                                        "provider_latency": {
                                            "component": "provider_latency",
                                            "actual_seconds": provider,
                                            "source": "trace.model_turns",
                                            "kind": "model_provider",
                                            "critical_path": True,
                                            "status": "measured",
                                            "notes": "",
                                        }
                                    },
                                },
                                "sandbox_breakdown": {
                                    "items": {
                                        "doctor_readiness": {
                                            "actual_seconds": sandbox / 2,
                                            "source": "sandbox_trace",
                                            "kind": "diagnostic_observation",
                                        },
                                        "command_runtime": {
                                            "actual_seconds": 1.0,
                                            "source": "sandbox_trace",
                                            "kind": "actual_execution",
                                        },
                                    }
                                },
                            },
                        }
                    ]
                }
            ),
            encoding="utf-8",
        )

    current = output_root / "run_c" / "result.json"
    compare = capability_repeated_timing_compare(current)

    assert compare["schema_version"] == "evaluation.capability_timing_compare/v1"
    assert compare["task_id"] == "task.public"
    assert compare["start_commit"] == "abc123"
    assert compare["run_count"] == 3
    assert compare["metrics"]["wall_time_seconds"] == {
        "current": 360.0,
        "min": 300.0,
        "median": 330.0,
    }
    assert compare["metrics"]["agent_loop_time_seconds"] == {
        "current": 240.0,
        "min": 200.0,
        "median": 220.0,
    }
    assert compare["metrics"]["dependency_setup_time_seconds"]["min"] == 30.0
    assert compare["metrics"]["sandbox_breakdown.doctor_readiness.actual_seconds"] == {
        "current": 30.0,
        "min": 25.0,
        "median": 30.0,
    }
    assert compare["metrics"]["sandbox_breakdown.command_runtime.actual_seconds"]["current"] == 1.0
    assert compare["metrics"]["latency_attribution.provider_latency.actual_seconds"] == {
        "current": 24.0,
        "min": 20.0,
        "median": 22.0,
    }


def test_capability_repeated_timing_compare_is_null_aware(tmp_path: Path) -> None:
    output_root = tmp_path / "work" / "evaluations"
    for run_id, provider in (("run_a", None), ("run_b", 2.0)):
        run_dir = output_root / run_id
        run_dir.mkdir(parents=True)
        (run_dir / "result.json").write_text(
            json.dumps(
                {
                    "tasks": [
                        {
                            "task_id": "task.public",
                            "reproducible_environment": {"workspace": {"start_commit": "abc123"}},
                            "timing": {
                                "wall_time_seconds": 10.0,
                                "provider_time_seconds": provider,
                            },
                            "capability_summary": {"wall_phases": {}},
                        }
                    ]
                }
            ),
            encoding="utf-8",
        )

    compare = capability_repeated_timing_compare(output_root / "run_b" / "result.json")

    assert compare["metrics"]["provider_time_seconds"] == {
        "current": 2.0,
        "min": 2.0,
        "median": 2.0,
    }


def test_capability_sla_diagnostics_classifies_jitter_without_changing_sla(tmp_path: Path) -> None:
    output_root = tmp_path / "work" / "evaluations"
    timings = [
        ("run_a", 263.0, 208.0, 50.0, 49.5),
        ("run_b", 264.0, 212.0, 60.0, 49.8),
        ("run_c", 263.899, 214.726, 62.922, 50.081),
    ]
    for run_id, wall, agent_loop, provider, sandbox in timings:
        run_dir = output_root / run_id
        run_dir.mkdir(parents=True)
        (run_dir / "result.json").write_text(
            json.dumps(
                {
                    "summary": {
                        "capability_sla": {
                            "schema_version": "evaluation.capability_sla_summary/v1",
                            "status": "over_sla",
                            "blocking": False,
                            "violations": {"agent_loop": 1, "provider": 1, "sandbox": 1},
                            "task_count": 1,
                        }
                    },
                    "tasks": [
                        {
                            "task_id": "task.public",
                            "reproducible_environment": {"workspace": {"start_commit": "abc123"}},
                            "timing": {
                                "wall_time_seconds": wall,
                                "provider_time_seconds": provider,
                                "sandbox_time_seconds": sandbox,
                            },
                            "capability_summary": {
                                "wall_phases": {"agent_loop_time_seconds": agent_loop},
                            },
                            "capability_sla": {
                                "schema_version": "evaluation.capability_sla/v1",
                                "status": "over_sla",
                                "blocking": False,
                                "violations": ["agent_loop", "provider", "sandbox"],
                                "items": {
                                    "agent_loop": {
                                        "actual_seconds": agent_loop,
                                        "target_seconds": 210.0,
                                        "status": "over_sla",
                                        "delta_seconds": round(agent_loop - 210.0, 3),
                                        "blocking": False,
                                    },
                                    "provider": {
                                        "actual_seconds": provider,
                                        "target_seconds": 55.0,
                                        "status": "over_sla",
                                        "delta_seconds": round(provider - 55.0, 3),
                                        "blocking": False,
                                    },
                                    "sandbox": {
                                        "actual_seconds": sandbox,
                                        "target_seconds": 50.0,
                                        "status": "over_sla",
                                        "delta_seconds": round(sandbox - 50.0, 3),
                                        "blocking": False,
                                    },
                                },
                            },
                        }
                    ],
                }
            ),
            encoding="utf-8",
        )

    result_path = output_root / "run_c" / "result.json"
    sla = capability_sla_from_result(result_path)
    compare = capability_repeated_timing_compare(result_path)
    diagnostics = capability_sla_diagnostics(sla, compare)

    assert sla["status"] == "over_sla"
    assert diagnostics["schema_version"] == "evaluation.capability_sla_diagnostics/v1"
    assert diagnostics["blocking"] is False
    assert diagnostics["items"]["sandbox"]["diagnosis"] == "timing_jitter"
    assert diagnostics["items"]["sandbox"]["median_seconds"] == 49.8
    assert diagnostics["items"]["sandbox"]["current_seconds"] == 50.081
    assert diagnostics["items"]["provider"]["diagnosis"] == "persistent_over_sla"
    assert diagnostics["items"]["agent_loop"]["diagnosis"] == "persistent_over_sla"
    capability_script = Path("scripts/verify_capability.py").read_text(encoding="utf-8")
    assert '"capability_sla_diagnostics": sla_diagnostics' in capability_script
    assert "_remaining_bottlenecks(capability_sla, sla_diagnostics)" in capability_script


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
    assert f"docs/evaluation/{'capability-' + 'regression-tasks.json'}" not in commands
    lowered = commands.lower()
    assert "fake provider" not in lowered
    assert "mock provider" not in lowered
    assert "scripted provider" not in lowered
