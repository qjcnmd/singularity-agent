from __future__ import annotations

import json
from pathlib import Path
from typing import Any
from uuid import uuid4

from singularity.evaluation.execution import (
    BenchmarkTaskExecutor,
    EvaluationArtifactWriter,
)
from singularity.evaluation.models import BenchmarkTask, EvaluationProfile
from singularity.evaluation.patch_quality import PatchQualityEvaluator
from singularity.evaluation.replay import TraceReplayHarness
from singularity.evaluation.reports import (
    EvaluationReport,
    ProfileEvaluationReport,
    RegressionReport,
    TaskEvaluationResult,
    metrics_for_results,
    new_report,
)
from singularity.evaluation.runner import (
    EvaluationRunner,
    EvaluationTaskSet,
    SingularityPrivateBenchmarkAdapter,
)
from singularity.evaluation.scoring import ScoringEngine


class EvaluationHarness:
    def __init__(
        self,
        *,
        project_root: Path | str,
        output_root: Path | str | None = None,
        trace_recorder: Any | None = None,
        verification_runner: Any | None = None,
        memory_pipeline: Any | None = None,
        planner: Any | None = None,
        tool_executor: Any | None = None,
        command_executor: Any | None = None,
        mutation_manager: Any | None = None,
    ) -> None:
        self.project_root = Path(project_root)
        self.output_root = Path(output_root) if output_root else self.project_root / "work" / "evaluations"
        self.trace_recorder = trace_recorder
        self.verification_runner = verification_runner
        self.memory_pipeline = memory_pipeline
        self.planner = planner
        self.tool_executor = tool_executor
        self.command_executor = command_executor
        self.mutation_manager = mutation_manager
        self.cancellation_token: Any | None = None
        self.scoring = ScoringEngine()
        self.patch_quality = PatchQualityEvaluator()
        self.trace_replay_harness = TraceReplayHarness(project_root=self.project_root)
        self.executor = BenchmarkTaskExecutor(
            project_root=self.project_root,
            command_executor=command_executor,
            verification_runner=verification_runner,
            mutation_manager=mutation_manager,
            trace_recorder=trace_recorder,
        )
        self.artifact_writer = EvaluationArtifactWriter(
            project_root=self.project_root,
            output_root=self.output_root,
            trace_recorder=trace_recorder,
        )

    def run_suite(
        self,
        *,
        tasks: list[BenchmarkTask],
        profiles: list[EvaluationProfile],
        trace_run_dir: Path | str | None = None,
        run_id: str | None = None,
        write_report: bool = False,
        execute: bool = False,
    ) -> EvaluationReport:
        run_id = run_id or f"eval_{uuid4().hex[:12]}"
        output_dir = self.output_root / run_id
        profile_reports: list[ProfileEvaluationReport] = []
        for profile in profiles:
            results = [
                self._evaluate_task(
                    task,
                    profile=profile,
                    trace_run_dir=trace_run_dir,
                    execute=execute,
                )
                for task in tasks
            ]
            profile_reports.append(
                ProfileEvaluationReport(
                    profile=profile,
                    task_results=results,
                    metrics=metrics_for_results(results),
                )
            )
        report = new_report(
            run_id=run_id,
            profile_reports=profile_reports,
            output_dir=output_dir,
        )
        if write_report:
            report = _with_previous_comparison(
                report,
                self.artifact_writer.previous_report_payload(run_id=run_id),
            )
            self.artifact_writer.write_report(
                run_id=run_id,
                json_text=report.to_json(),
                markdown_text=report.to_markdown(),
            )
        return report

    def run_ab(
        self,
        *,
        tasks: list[BenchmarkTask],
        baseline: EvaluationProfile,
        candidate: EvaluationProfile,
        trace_run_dir: Path | str | None = None,
        run_id: str | None = None,
        write_report: bool = False,
        execute: bool = False,
    ) -> EvaluationReport:
        return self.run_suite(
            tasks=tasks,
            profiles=[baseline, candidate],
            trace_run_dir=trace_run_dir,
            run_id=run_id,
            write_report=write_report,
            execute=execute,
        )

    def write_regression_report(
        self,
        *,
        run_id: str,
        regression: RegressionReport,
    ) -> Path:
        return self.artifact_writer.write_regression_report(
            run_id=run_id,
            json_text=regression.to_json(),
            markdown_text=regression.to_markdown(),
        )

    def run_agent_benchmark_suite(
        self,
        *,
        manifest: EvaluationTaskSet,
        run_id: str | None = None,
        output_root: Path | str | None = None,
        max_turns: int | None = None,
        model: str | None = None,
        base_url: str | None = None,
        baseline_result_path: Path | str | None = None,
        bootstrap_cls: Any | None = None,
    ) -> dict[str, Any]:
        """Run coding-agent benchmarks through KernelBootstrap -> AgentKernel -> AgentLoop."""
        return EvaluationRunner(
            output_root=output_root or self.output_root,
            run_id=run_id,
            max_turns=max_turns,
            model=model,
            base_url=base_url,
            baseline_result_path=baseline_result_path,
            bootstrap_cls=bootstrap_cls,
        ).run(manifest)

    def run_private_agent_benchmark_suite(
        self,
        *,
        task_set: Path | str,
        run_id: str | None = None,
        output_root: Path | str | None = None,
        max_turns: int | None = None,
        model: str | None = None,
        base_url: str | None = None,
        baseline_result_path: Path | str | None = None,
        bootstrap_cls: Any | None = None,
    ) -> dict[str, Any]:
        manifest = SingularityPrivateBenchmarkAdapter().load(task_set)
        return self.run_agent_benchmark_suite(
            manifest=manifest,
            run_id=run_id,
            output_root=output_root,
            max_turns=max_turns,
            model=model,
            base_url=base_url,
            baseline_result_path=baseline_result_path,
            bootstrap_cls=bootstrap_cls,
        )

    def _evaluate_task(
        self,
        task: BenchmarkTask,
        *,
        profile: EvaluationProfile,
        trace_run_dir: Path | str | None,
        execute: bool,
    ) -> TaskEvaluationResult:
        replay = None
        execution_evidence: dict[str, Any] = {}
        score_delta = 0.0
        if trace_run_dir is not None:
            replay = self.trace_replay_harness.replay(trace_run_dir, profile=profile)
            verification = replay.verification
            assertions = {}
            diff = {}
            trace_metrics = replay.metrics
            diff_summary: list[dict[str, Any]] = []
            heuristics = {
                "planner_completion": 1.0
                if verification.get("status") in {"ready", "passed"}
                else 0.0
            }
        else:
            evidence = self.executor.evaluate(
                task,
                agent_config_overrides=profile.to_agent_config_overrides(),
                execute=execute,
            )
            execution_evidence = evidence.to_dict()
            verification = evidence.verification
            assertions = evidence.assertions
            diff = evidence.diff
            trace_metrics = {
                **evidence.trace_metrics,
                "failure_reasons": evidence.failure_reasons,
            }
            diff_summary = evidence.diff_summary
            heuristics = dict(evidence.heuristics)
            score_delta = _score_adjustment_delta(evidence.hook_results)
        patch_quality = self.patch_quality.evaluate(
            diff_summary=diff_summary,
            verification=verification,
        )
        heuristics.setdefault("patch_quality", patch_quality.score)
        heuristics.setdefault(
            "planner_completion",
            1.0 if verification.get("status") in {"ready", "passed", "not_required"} else 0.0,
        )
        scoring = self.scoring.score(
            task=task,
            verification=verification,
            assertions=assertions,
            diff=diff,
            heuristics=heuristics,
            trace_metrics=trace_metrics,
        )
        scoring = _apply_score_delta(scoring, score_delta)
        if trace_run_dir is None:
            scoring = _apply_execution_failures(scoring, execution_evidence)
        return TaskEvaluationResult(
            task_id=task.task_id,
            profile=profile,
            scoring=scoring,
            replay=replay,
            patch_quality=patch_quality.to_dict(),
            agent_config_overrides=profile.to_agent_config_overrides(),
            execution_evidence=execution_evidence,
            latency_ms=int(trace_metrics.get("latency_ms", 0) or 0),
            cost=float(trace_metrics.get("cost", 0.0) or 0.0),
            tool_calls=int(trace_metrics.get("tool_calls", 0) or 0),
            intervention_count=int(trace_metrics.get("interventions", 0) or 0),
        )


class RegressionDetector:
    def compare(
        self,
        baseline: ProfileEvaluationReport,
        candidate: ProfileEvaluationReport,
        *,
        threshold: float = 0.05,
        block_on_regression: bool = False,
    ) -> RegressionReport:
        regressions: list[dict[str, Any]] = []
        task_diffs: list[dict[str, Any]] = []
        lower_is_better = {"cost", "latency_ms", "tool_calls", "intervention_rate"}
        metrics = sorted(set(baseline.metrics) & set(candidate.metrics))
        for metric in metrics:
            if metric in {"task_count", "success_count", "failure_count", "intervention_count"}:
                continue
            base = baseline.metrics.get(metric, 0)
            cand = candidate.metrics.get(metric, 0)
            if not isinstance(base, int | float) or not isinstance(cand, int | float):
                continue
            delta = cand - base
            regressed = (
                delta > max(threshold, abs(base) * threshold)
                if metric in lower_is_better
                else delta < -max(threshold, abs(base) * threshold)
            )
            if regressed:
                regressions.append(
                    {
                        "metric": metric,
                        "task_id": "__suite__",
                        "baseline": base,
                        "candidate": cand,
                        "delta": round(delta, 6),
                        "trace_artifact_ref": _regression_artifact_ref(
                            task_id="__suite__",
                            metric=metric,
                            baseline_profile=baseline.profile.name,
                            candidate_profile=candidate.profile.name,
                        ),
                    }
                )
        baseline_by_task = {item.task_id: item for item in baseline.task_results}
        for candidate_task in candidate.task_results:
            baseline_task = baseline_by_task.get(candidate_task.task_id)
            if baseline_task is None:
                continue
            task_diffs.append(
                {
                    "task_id": candidate_task.task_id,
                    "baseline_status": baseline_task.scoring.status,
                    "candidate_status": candidate_task.scoring.status,
                    "baseline_score": baseline_task.scoring.score,
                    "candidate_score": candidate_task.scoring.score,
                    "score_delta": round(
                        candidate_task.scoring.score - baseline_task.scoring.score,
                        6,
                    ),
                }
            )
            if (
                baseline_task.scoring.status == "success"
                and candidate_task.scoring.status != "success"
            ):
                regressions.append(
                    {
                        "metric": f"task:{candidate_task.task_id}:status",
                        "task_id": candidate_task.task_id,
                        "baseline": baseline_task.scoring.status,
                        "candidate": candidate_task.scoring.status,
                        "delta": "success_to_failure",
                        "trace_artifact_ref": _regression_artifact_ref(
                            task_id=candidate_task.task_id,
                            metric="status",
                            baseline_profile=baseline.profile.name,
                            candidate_profile=candidate.profile.name,
                        ),
                    }
                )
            elif candidate_task.scoring.score + threshold < baseline_task.scoring.score:
                regressions.append(
                    {
                        "metric": f"task:{candidate_task.task_id}:score",
                        "task_id": candidate_task.task_id,
                        "baseline": baseline_task.scoring.score,
                        "candidate": candidate_task.scoring.score,
                        "delta": round(candidate_task.scoring.score - baseline_task.scoring.score, 6),
                        "trace_artifact_ref": _regression_artifact_ref(
                            task_id=candidate_task.task_id,
                            metric="score",
                            baseline_profile=baseline.profile.name,
                            candidate_profile=candidate.profile.name,
                        ),
                    }
                )
        return RegressionReport(
            baseline_profile=baseline.profile.name,
            candidate_profile=candidate.profile.name,
            threshold=threshold,
            blocking=bool(block_on_regression and regressions),
            regressions=regressions,
            task_diffs=task_diffs,
            summary={
                "baseline": baseline.metrics,
                "candidate": candidate.metrics,
                "regression_count": len(regressions),
            },
        )


def _score_adjustment_delta(hook_results: list[dict[str, Any]]) -> float:
    delta = 0.0
    for result in hook_results:
        if result.get("stage") != "score_adjustment":
            continue
        if result.get("error_code") or str(result.get("status", "")).lower() in {
            "blocked",
            "policy_blocked",
            "review_required",
            "failed",
        }:
            continue
        args = result.get("args") or {}
        delta += _bounded_delta(args.get("score_delta"))
        output = str(result.get("output_excerpt") or "")
        if not output.strip():
            continue
        try:
            payload = json.loads(output)
        except Exception:
            continue
        if isinstance(payload, dict):
            delta += _bounded_delta(payload.get("score_delta"))
    return max(-1.0, min(1.0, delta))


def _regression_artifact_ref(
    *,
    task_id: str,
    metric: str,
    baseline_profile: str,
    candidate_profile: str,
) -> str:
    safe = "|".join([baseline_profile, candidate_profile, task_id, metric])
    import hashlib

    digest = hashlib.sha256(safe.encode("utf-8")).hexdigest()[:16]
    return f"regression:{task_id}:{metric}:{digest}"


def _bounded_delta(value: Any) -> float:
    try:
        delta = float(value or 0.0)
    except (TypeError, ValueError):
        return 0.0
    return max(-1.0, min(1.0, delta))


def _apply_execution_failures(
    scoring: Any,
    execution_evidence: dict[str, Any],
) -> Any:
    failures = list(execution_evidence.get("failure_reasons") or [])
    if not failures:
        return scoring
    existing = list(scoring.failure_reasons)
    merged = list(dict.fromkeys([*existing, *failures]))
    return type(scoring)(
        task_id=scoring.task_id,
        status="failure",
        score=scoring.score,
        subscores=scoring.subscores,
        evidence=scoring.evidence,
        failure_reasons=merged,
    )


def _apply_score_delta(scoring: Any, delta: float) -> Any:
    if not delta:
        return scoring
    score = round(max(0.0, min(1.0, scoring.score + delta)), 4)
    evidence = [
        *scoring.evidence,
        {
            "kind": "hook",
            "source": "score_adjustment",
            "score_delta": round(delta, 4),
            "score": score,
        },
    ]
    status = "success" if score >= 0.5 and not scoring.failure_reasons else "failure"
    return type(scoring)(
        task_id=scoring.task_id,
        status=status,
        score=score,
        subscores=scoring.subscores,
        evidence=evidence,
        failure_reasons=scoring.failure_reasons,
    )


def _with_previous_comparison(
    report: EvaluationReport,
    previous: dict[str, Any] | None,
) -> EvaluationReport:
    if not previous:
        return report
    previous_metrics = previous.get("metrics")
    if not isinstance(previous_metrics, dict):
        return report
    metrics = dict(report.metrics)
    metrics["previous_comparison"] = {
        "previous_run_id": str(previous.get("run_id") or ""),
        "success_rate_delta": _metric_delta(metrics, previous_metrics, "success_rate"),
        "average_score_delta": _metric_delta(metrics, previous_metrics, "average_score"),
        "cost_delta": _metric_delta(metrics, previous_metrics, "cost"),
        "latency_ms_delta": _metric_delta(metrics, previous_metrics, "latency_ms"),
        "tool_calls_delta": _metric_delta(metrics, previous_metrics, "tool_calls"),
    }
    return EvaluationReport(
        run_id=report.run_id,
        generated_at=report.generated_at,
        profile_reports=report.profile_reports,
        metrics=metrics,
        output_dir=report.output_dir,
    )


def _metric_delta(current: dict[str, Any], previous: dict[str, Any], key: str) -> float | int:
    delta = float(current.get(key, 0) or 0) - float(previous.get(key, 0) or 0)
    if key in {"latency_ms", "tool_calls"}:
        return int(delta)
    return round(delta, 6)
