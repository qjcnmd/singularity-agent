from __future__ import annotations

import json

from singularity.evaluation.models import EvaluationProfile, ScoringResult
from singularity.evaluation.reports import (
    EvaluationReport,
    ProfileEvaluationReport,
    RegressionReport,
    TaskEvaluationResult,
)


def _profile() -> EvaluationProfile:
    return EvaluationProfile(
        name="baseline",
        model="test-model",
        prompt_profile="default",
        memory_enabled=False,
        allowed_tools=["read_file"],
        tool_policy="read_only",
    )


def _task_result() -> TaskEvaluationResult:
    return TaskEvaluationResult(
        task_id="contract.task",
        profile=_profile(),
        scoring=ScoringResult(
            task_id="contract.task",
            status="success",
            score=0.75,
            subscores={"verification": 1.0, "patch": 0.5},
            evidence=[{"kind": "verification", "observed": True}],
            failure_reasons=[],
        ),
        patch_quality={"score": 0.5},
        agent_config_overrides={"model": "test-model"},
        execution_evidence={
            "golden_contract": {
                "expected_files": [{"path": "src/app.py", "observed": True}],
                "expected_commands": [{"command": "python -m pytest", "observed": True}],
                "expected_evidence": [{"name": "tests_passed", "observed": True}],
                "expected_report_sections": [{"section": "Verification", "observed": True}],
                "required_trace_artifacts": [{"kind": "events", "observed": True}],
            }
        },
        latency_ms=123,
        cost=0.0123,
        tool_calls=2,
        intervention_count=0,
    )


def test_evaluation_report_json_and_markdown_contract(tmp_path) -> None:
    profile_report = ProfileEvaluationReport(
        profile=_profile(),
        task_results=[_task_result()],
        metrics={
            "success_rate": 1.0,
            "average_score": 0.75,
            "cost": 0.0123,
            "latency_ms": 123,
            "tool_calls": 2,
            "intervention_rate": 0.0,
        },
    )
    report = EvaluationReport(
        run_id="report_contract",
        generated_at="2026-07-03T00:00:00+00:00",
        profile_reports=[profile_report],
        metrics={
            "success_rate": 1.0,
            "average_score": 0.75,
            "cost": 0.0123,
            "latency_ms": 123,
            "tool_calls": 2,
            "intervention_rate": 0.0,
            "failure_taxonomy": {},
        },
        output_dir=tmp_path / "reports",
    )

    payload = report.to_dict()
    markdown = report.to_markdown()

    assert payload["schema_version"] == "evaluation.report/v1"
    assert payload["run_id"] == "report_contract"
    assert payload["generated_at"] == "2026-07-03T00:00:00+00:00"
    assert payload["output_dir"] == str(tmp_path / "reports")
    assert len(payload["report_hash"]) == 64
    assert payload["profile_reports"][0]["task_results"][0]["execution_evidence"][
        "golden_contract"
    ]["expected_files"][0]["path"] == "src/app.py"
    assert json.loads(report.to_json()) == payload
    assert "# Evaluation Report `report_contract`" in markdown
    assert "## Profiles" in markdown
    assert "### baseline" in markdown
    assert "| `contract.task` | success | 0.75 | - |" in markdown
    assert "## Golden Task Evidence" in markdown
    assert "| `baseline` | `contract.task` | src/app.py | python -m pytest | tests_passed | Verification | events |" in markdown


def test_regression_report_json_and_markdown_contract(tmp_path) -> None:
    report = RegressionReport(
        baseline_profile="baseline",
        candidate_profile="candidate",
        threshold=0.1,
        blocking=True,
        regressions=[
            {
                "metric": "score",
                "baseline": 0.9,
                "candidate": 0.7,
                "delta": -0.2,
                "trace_artifact_ref": "trace/events.jsonl",
            }
        ],
        summary={"regression_count": 1},
        task_diffs=[
            {
                "task_id": "contract.task",
                "baseline_status": "success",
                "baseline_score": 0.9,
                "candidate_status": "failed",
                "candidate_score": 0.7,
                "score_delta": -0.2,
            }
        ],
    )

    payload = report.to_dict()
    markdown = report.to_markdown()
    report.write(tmp_path)

    assert payload["schema_version"] == "evaluation.regression_report/v1"
    assert json.loads(report.to_json()) == payload
    assert (tmp_path / "regression.json").exists()
    assert (tmp_path / "regression.md").exists()
    assert "# Regression Report `baseline` vs `candidate`" in markdown
    assert "| score | 0.9 | 0.7 | -0.2 | trace/events.jsonl |" in markdown
    assert "## Per-Task Diff" in markdown
    assert "| `contract.task` | success (0.9) | failed (0.7) | -0.2 |" in markdown
