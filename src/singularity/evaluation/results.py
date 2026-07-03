from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

EVALUATION_RESULT_SCHEMA_VERSION = "evaluation.result/v1"


@dataclass(frozen=True)
class CommandEvalResult:
    command: str
    exit_code: int | None
    duration_seconds: float
    timed_out: bool = False
    error_summary: str = ""
    raw_command: str = ""
    resolved_argv: list[str] = field(default_factory=list)
    interpreter_strategy: dict[str, Any] = field(default_factory=dict)
    failure_category: str = ""

    @property
    def passed(self) -> bool:
        return self.exit_code == 0 and not self.timed_out

    def to_dict(self) -> dict[str, Any]:
        return {
            "command": self.command,
            "exit_code": self.exit_code,
            "duration_seconds": self.duration_seconds,
            "timed_out": self.timed_out,
            "passed": self.passed,
            "error_summary": self.error_summary,
            "raw_command": self.raw_command or self.command,
            "resolved_argv": list(self.resolved_argv),
            "interpreter_strategy": dict(self.interpreter_strategy),
            "failure_category": self.failure_category,
        }


@dataclass(frozen=True)
class EvaluationTaskResult:
    task_id: str
    tests_passed: bool
    infrastructure_blocked: bool
    prompt_tokens: int
    cached_tokens: int
    request_cache_hit_rate: float
    run_cache_hit_rate: float
    tool_calls: int
    files_changed: list[str]
    duration_seconds: float
    error_summary: str
    workspace: str
    trace: str
    verification_workspace: str = ""
    patch: dict[str, Any] = field(default_factory=dict)
    checks: dict[str, Any] = field(default_factory=dict)
    verification: CommandEvalResult | None = None
    agent_completed: bool = False
    evaluation_passed: bool = False
    patch_applicable: bool = False
    allowed_scope_passed: bool = False
    public_verification_passed: bool = False
    hidden_verification_passed: bool = False
    sandbox_enforcement_passed: bool = True
    evaluator_visibility_audit_passed: bool = True
    local_process_fallback_count: int = 0
    repair_attempt_count: int = 0
    repair_execution_count: int = 0
    miscompletion_count: int = 0
    blocked_reason: str = ""
    failure_category: str = ""
    request_cache_hit_rates: dict[str, float] = field(default_factory=dict)
    status: str = "unknown"
    turn_count: int = 0
    verification_result: dict[str, Any] = field(default_factory=dict)
    contract_satisfaction: dict[str, Any] = field(default_factory=dict)
    final_report_status: str = ""
    policy_blocks: int = 0
    token_usage: dict[str, Any] = field(default_factory=dict)
    cache_usage: dict[str, Any] = field(default_factory=dict)
    trace_artifact_refs: list[str] = field(default_factory=list)
    reproducible_environment: dict[str, Any] = field(default_factory=dict)
    capability_summary: dict[str, Any] = field(default_factory=dict)
    capability_sla: dict[str, Any] = field(default_factory=dict)
    timing: dict[str, Any] = field(default_factory=dict)
    baseline_failed: bool = False
    baseline_checks: dict[str, Any] = field(default_factory=dict)
    patch_applied: bool = False
    fail_to_pass_satisfied: bool = False
    verification_misconfiguration_reason: str = ""
    evaluation_metrics: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "task_id": self.task_id,
            "status": self.status,
            "tests_passed": self.tests_passed,
            "infrastructure_blocked": self.infrastructure_blocked,
            "turn_count": self.turn_count,
            "prompt_tokens": self.prompt_tokens,
            "cached_tokens": self.cached_tokens,
            "request_cache_hit_rate": self.request_cache_hit_rate,
            "run_cache_hit_rate": self.run_cache_hit_rate,
            "tool_calls": self.tool_calls,
            "files_changed": list(self.files_changed),
            "duration_seconds": self.duration_seconds,
            "error_summary": self.error_summary,
            "workspace": self.workspace,
            "trace": self.trace,
            "verification_workspace": self.verification_workspace,
            "patch": self.patch,
            "checks": self.checks,
            "verification": self.verification.to_dict() if self.verification else None,
            "agent_completed": self.agent_completed,
            "evaluation_passed": self.evaluation_passed,
            "patch_applicable": self.patch_applicable,
            "allowed_scope_passed": self.allowed_scope_passed,
            "public_verification_passed": self.public_verification_passed,
            "hidden_verification_passed": self.hidden_verification_passed,
            "sandbox_enforcement_passed": self.sandbox_enforcement_passed,
            "evaluator_visibility_audit_passed": self.evaluator_visibility_audit_passed,
            "local_process_fallback_count": self.local_process_fallback_count,
            "repair_attempt_count": self.repair_attempt_count,
            "repair_execution_count": self.repair_execution_count,
            "miscompletion_count": self.miscompletion_count,
            "blocked_reason": self.blocked_reason,
            "failure_category": self.failure_category,
            "request_cache_hit_rates": dict(sorted(self.request_cache_hit_rates.items())),
            "verification_result": self.verification_result,
            "contract_satisfaction": self.contract_satisfaction,
            "final_report_status": self.final_report_status,
            "policy_blocks": self.policy_blocks,
            "token_usage": self.token_usage,
            "cache_usage": self.cache_usage,
            "trace_artifact_refs": list(self.trace_artifact_refs),
            "reproducible_environment": self.reproducible_environment,
            "capability_summary": self.capability_summary,
            "capability_sla": self.capability_sla,
            "timing": self.timing,
            "baseline_failed": self.baseline_failed,
            "baseline_checks": self.baseline_checks,
            "patch_applied": self.patch_applied,
            "fail_to_pass_satisfied": self.fail_to_pass_satisfied,
            "verification_misconfiguration_reason": self.verification_misconfiguration_reason,
            "evaluation_metrics": self.evaluation_metrics,
        }


def summarize_evaluation_results(results: list[EvaluationTaskResult]) -> dict[str, Any]:
    task_count = len(results)
    scored_results = [result for result in results if not result.infrastructure_blocked]
    scored_task_count = len(scored_results)
    infrastructure_blocked_count = task_count - scored_task_count
    evaluation_passed_count = sum(1 for result in results if result.evaluation_passed)
    tests_passed_count = sum(1 for result in results if result.tests_passed)
    prompt_tokens = sum(result.prompt_tokens for result in results)
    cached_tokens = sum(result.cached_tokens for result in results)
    resolved_count = sum(
        1 for result in scored_results if (result.evaluation_metrics.get("resolved") or {}).get("value") is True
    )
    fail_to_pass_satisfied_count = sum(
        1
        for result in scored_results
        if ((result.evaluation_metrics.get("swe_bench") or {}).get("fail_to_pass") or {}).get("satisfied") is True
    )
    pass_to_pass_satisfied_count = sum(
        1
        for result in scored_results
        if ((result.evaluation_metrics.get("swe_bench") or {}).get("pass_to_pass") or {}).get("satisfied") is True
    )
    pass_to_pass_not_configured_count = sum(
        1
        for result in results
        if ((result.evaluation_metrics.get("swe_bench") or {}).get("pass_to_pass") or {}).get("status")
        == "not_configured"
    )
    tool_success_rates = [
        _safe_float((result.evaluation_metrics.get("tools") or {}).get("tool_success_rate"))
        for result in scored_results
        if (result.evaluation_metrics.get("tools") or {}).get("tool_success_rate") is not None
    ]
    cost_estimates = [
        _safe_float((result.evaluation_metrics.get("cost") or {}).get("cost_estimate"))
        for result in results
        if (result.evaluation_metrics.get("cost") or {}).get("cost_estimate") is not None
    ]
    scored_cost_estimates = [
        _safe_float((result.evaluation_metrics.get("cost") or {}).get("cost_estimate"))
        for result in scored_results
        if (result.evaluation_metrics.get("cost") or {}).get("cost_estimate") is not None
    ]
    total_cost_estimate = round(sum(cost_estimates), 6)
    scored_cost_estimate = round(sum(scored_cost_estimates), 6)
    failures: dict[str, int] = {}
    miscompletion_count = 0
    for result in results:
        if not result.evaluation_passed:
            reason = result.failure_category if result.failure_category and result.failure_category != "none" else result.status
            failures[reason or "failure"] = failures.get(reason or "failure", 0) + 1
    for result in scored_results:
        miscompletion_count += result.miscompletion_count or int(
            result.agent_completed and not result.evaluation_passed
        )
    agent_completed_count = sum(1 for result in scored_results if result.agent_completed)
    capability_sla = _summarize_capability_sla(results)
    return {
        "task_count": task_count,
        "scored_task_count": scored_task_count,
        "infrastructure_blocked_count": infrastructure_blocked_count,
        "score_status": _score_status(
            task_count=task_count,
            scored_task_count=scored_task_count,
            infrastructure_blocked_count=infrastructure_blocked_count,
        ),
        "task_completion_rate": _rate(agent_completed_count, scored_task_count),
        "tests_passed_count": tests_passed_count,
        "test_pass_rate": _rate(tests_passed_count, scored_task_count),
        "prompt_tokens": prompt_tokens,
        "cached_tokens": cached_tokens,
        "request_cache_hit_rate": _average_rate({result.task_id: result.request_cache_hit_rate for result in scored_results}),
        "run_cache_hit_rate": _rate(cached_tokens, prompt_tokens),
        "tool_calls": sum(result.tool_calls for result in results),
        "resolved_count": resolved_count,
        "resolved_rate": _rate(resolved_count, scored_task_count),
        "fail_to_pass_satisfied_count": fail_to_pass_satisfied_count,
        "pass_to_pass_satisfied_count": pass_to_pass_satisfied_count,
        "pass_to_pass_not_configured_count": pass_to_pass_not_configured_count,
        "average_tool_success_rate": round(sum(tool_success_rates) / len(tool_success_rates), 4)
        if tool_success_rates
        else None,
        "total_cost_estimate": total_cost_estimate,
        "cost_per_resolved": round(scored_cost_estimate / resolved_count, 6) if resolved_count else None,
        "evaluation_passed_rate": _rate(evaluation_passed_count, scored_task_count),
        "verification_pass_rate": _rate(tests_passed_count, scored_task_count),
        "average_turns": round(
            sum(result.turn_count for result in scored_results) / scored_task_count,
            4,
        )
        if scored_task_count
        else 0.0,
        "average_tool_calls": round(
            sum(result.tool_calls for result in scored_results) / scored_task_count,
            4,
        )
        if scored_task_count
        else 0.0,
        "agent_completed_count": agent_completed_count,
        "evaluation_passed_count": evaluation_passed_count,
        "repair_attempt_count": sum(result.repair_attempt_count for result in results),
        "repair_execution_count": sum(result.repair_execution_count for result in results),
        "policy_blocks": sum(result.policy_blocks for result in results),
        "miscompletion_count": miscompletion_count,
        "failure_reasons": dict(sorted(failures.items())),
        "capability_sla": capability_sla,
    }


def compare_evaluation_results(
    baseline: dict[str, Any],
    candidate: dict[str, Any],
) -> dict[str, Any]:
    baseline_summary = _dict(baseline.get("summary") or {}, "baseline.summary")
    candidate_summary = _dict(candidate.get("summary") or {}, "candidate.summary")
    task_diffs: list[dict[str, Any]] = []
    baseline_tasks = {
        str(item.get("task_id")): item
        for item in baseline.get("tasks") or []
        if isinstance(item, dict)
    }
    for item in candidate.get("tasks") or []:
        if not isinstance(item, dict):
            continue
        task_id = str(item.get("task_id") or "")
        previous = baseline_tasks.get(task_id)
        if not previous:
            continue
        task_diffs.append(
            {
                "task_id": task_id,
                "baseline_status": str(previous.get("status") or ""),
                "candidate_status": str(item.get("status") or ""),
                "baseline_success": _evaluation_passed_from_payload(previous),
                "candidate_success": _evaluation_passed_from_payload(item),
                "turn_delta": _safe_int(item.get("turn_count")) - _safe_int(previous.get("turn_count")),
                "tool_call_delta": _payload_tool_calls(item) - _payload_tool_calls(previous),
                "verification_changed": bool(previous.get("tests_passed")) != bool(item.get("tests_passed")),
                "trace_artifact_refs": list(item.get("trace_artifact_refs") or []),
            }
        )
    regressions = [
        diff
        for diff in task_diffs
        if diff["baseline_success"] and not diff["candidate_success"]
    ]
    return {
        "schema_version": "evaluation.regression/v1",
        "baseline_run_id": str(baseline.get("run_id") or ""),
        "candidate_run_id": str(candidate.get("run_id") or ""),
        "summary": {
            "evaluation_passed_rate_delta": _float_delta(
                candidate_summary,
                baseline_summary,
                "evaluation_passed_rate",
            ),
            "verification_pass_rate_delta": _float_delta(candidate_summary, baseline_summary, "verification_pass_rate"),
            "average_turns_delta": _float_delta(candidate_summary, baseline_summary, "average_turns"),
            "average_tool_calls_delta": _float_delta(candidate_summary, baseline_summary, "average_tool_calls"),
            "policy_blocks_delta": _safe_int(candidate_summary.get("policy_blocks"))
            - _safe_int(baseline_summary.get("policy_blocks")),
            "miscompletion_delta": _safe_int(candidate_summary.get("miscompletion_count"))
            - _safe_int(baseline_summary.get("miscompletion_count")),
            "regression_count": len(regressions),
        },
        "task_diffs": task_diffs,
        "regressions": regressions,
    }


def evaluation_report_markdown(payload: dict[str, Any]) -> str:
    summary = _dict(payload.get("summary") or {}, "summary")
    lines = [
        f"# Agent Evaluation `{payload.get('run_id', '')}`",
        "",
        f"- status: `{summary.get('score_status', 'unknown')}`",
        f"- task count: {summary.get('task_count', 0)}",
        f"- evaluation passed rate: {summary.get('evaluation_passed_rate', 0):.4f}",
        f"- verification pass rate: {summary.get('verification_pass_rate', 0):.4f}",
        f"- average turns: {summary.get('average_turns', 0):.4f}",
        f"- average tool calls: {summary.get('average_tool_calls', 0):.4f}",
        f"- policy blocks: {summary.get('policy_blocks', 0)}",
        f"- repair attempts: {summary.get('repair_attempt_count', 0)}",
        f"- repair executions: {summary.get('repair_execution_count', 0)}",
        f"- miscompletion count: {summary.get('miscompletion_count', 0)}",
        "",
        "## Metrics / Scorecard",
        "",
        f"- resolved: {summary.get('resolved_count', 0)} / {summary.get('scored_task_count', 0)} ({summary.get('resolved_rate', 0):.4f})",
        f"- FAIL_TO_PASS satisfied: {summary.get('fail_to_pass_satisfied_count', 0)}",
        f"- PASS_TO_PASS satisfied: {summary.get('pass_to_pass_satisfied_count', 0)}",
        f"- PASS_TO_PASS not configured: {summary.get('pass_to_pass_not_configured_count', 0)}",
        f"- average tool success rate: {_format_optional_rate(summary.get('average_tool_success_rate'))}",
        f"- total cost estimate: {_format_optional_float(summary.get('total_cost_estimate'))}",
        f"- cost per resolved: {_format_optional_float(summary.get('cost_per_resolved'))}",
        "",
        "| task | status | verification | turns | tools | files changed | final report | contract | failures |",
        "| --- | --- | --- | ---: | ---: | --- | --- | --- | --- |",
    ]
    for task in payload.get("tasks") or []:
        if not isinstance(task, dict):
            continue
        files = ", ".join(str(item) for item in task.get("files_changed") or []) or "-"
        failures = str(task.get("error_summary") or "-").replace("|", "\\|")
        contract = _dict(task.get("contract_satisfaction") or {}, "contract").get("status", "unknown")
        verification = _dict(task.get("verification_result") or {}, "verification").get("status", "unknown")
        lines.append(
            "| "
            f"`{task.get('task_id', '')}` | "
            f"{task.get('status') or 'unknown'} | "
            f"{verification} | "
            f"{_safe_int(task.get('turn_count'))} | "
            f"{_safe_int(task.get('tool_calls'))} | "
            f"{files} | "
            f"{task.get('final_report_status') or '-'} | "
            f"{contract} | "
            f"{failures} |"
        )
    scorecard_rows: list[str] = []
    for task in payload.get("tasks") or []:
        if not isinstance(task, dict):
            continue
        metrics = task.get("evaluation_metrics") or {}
        if not isinstance(metrics, dict):
            continue
        resolved = metrics.get("resolved") or {}
        swe_bench = metrics.get("swe_bench") or {}
        fail_to_pass = swe_bench.get("fail_to_pass") if isinstance(swe_bench, dict) else {}
        pass_to_pass = swe_bench.get("pass_to_pass") if isinstance(swe_bench, dict) else {}
        verification = metrics.get("verification") or {}
        patch = metrics.get("patch") or {}
        trajectory = metrics.get("trajectory") or {}
        tools = metrics.get("tools") or {}
        context = metrics.get("context") or {}
        efficiency = metrics.get("efficiency") or {}
        cost = metrics.get("cost") or {}
        safety = metrics.get("safety") or {}
        failure_reason = str((resolved or {}).get("reason") or task.get("failure_category") or "-").replace("|", "\\|")
        scorecard_rows.append(
            "| "
            f"`{task.get('task_id', '')}` | "
            f"{bool((resolved or {}).get('value'))} | "
            f"{(fail_to_pass or {}).get('satisfied')} | "
            f"{(pass_to_pass or {}).get('status') or (pass_to_pass or {}).get('satisfied')} | "
            f"{(verification or {}).get('tests_passed')} | "
            f"{(patch or {}).get('files_changed_count')} / {(patch or {}).get('out_of_scope_files') or []} | "
            f"{(trajectory or {}).get('turn_count')} / {(tools or {}).get('tool_call_count')} / {(tools or {}).get('tool_success_rate')} | "
            f"{((context or {}).get('compaction') or {}).get('reason') or ((context or {}).get('compaction') or {}).get('status') or '-'} | "
            f"{(efficiency or {}).get('wall_time_seconds')} | "
            f"{_format_optional_float((cost or {}).get('cost_estimate'))} ({(cost or {}).get('cost_source', 'unknown')}) | "
            f"{(safety or {}).get('policy_blocks')} | "
            f"{failure_reason} |"
        )
    if scorecard_rows:
        lines.extend(
            [
                "",
                "| task | resolved | FAIL_TO_PASS | PASS_TO_PASS | verification | patch files / out of scope | turns / tools / success | context / compaction | wall seconds | cost | policy blocks | failure reason |",
                "| --- | --- | --- | --- | --- | --- | --- | --- | ---: | --- | ---: | --- |",
                *scorecard_rows,
            ]
        )
    timing_rows: list[str] = []
    for task in payload.get("tasks") or []:
        if not isinstance(task, dict):
            continue
        timing = task.get("timing") or {}
        capability = task.get("capability_summary") or {}
        diagnostics = capability.get("timing_diagnostics") if isinstance(capability, dict) else {}
        diagnostics = diagnostics if isinstance(diagnostics, dict) else {}
        if not isinstance(timing, dict):
            continue
        for name, value in sorted(timing.items()):
            diagnostic = diagnostics.get(name) if isinstance(diagnostics.get(name), dict) else {}
            status = str(diagnostic.get("status") or ("measured" if value is not None else "unavailable"))
            reason = str(diagnostic.get("reason") or "-").replace("|", "\\|")
            rendered_value = "-" if value is None else str(value)
            timing_rows.append(
                f"| `{task.get('task_id', '')}` | `{name}` | {rendered_value} | {status} | {reason} |"
            )
    if timing_rows:
        lines.extend(
            [
                "",
                "## Timing Breakdown",
                "",
                "| task | metric | seconds | status | reason |",
                "| --- | --- | ---: | --- | --- |",
                *timing_rows,
            ]
        )
    sla_rows: list[str] = []
    for task in payload.get("tasks") or []:
        if not isinstance(task, dict):
            continue
        sla = task.get("capability_sla") or {}
        if not isinstance(sla, dict):
            continue
        for name, item in sorted((sla.get("items") or {}).items()):
            if not isinstance(item, dict):
                continue
            actual = item.get("actual_seconds")
            target = item.get("target_seconds")
            if actual is None and "actual_count" in item:
                actual = item.get("actual_count")
                target = item.get("target_count")
            if actual is None and "passed" in item:
                actual = item.get("passed")
                target = True
            sla_rows.append(
                "| "
                f"`{task.get('task_id', '')}` | "
                f"`{name}` | "
                f"{actual} | "
                f"{target} | "
                f"{item.get('status') or 'unknown'} | "
                f"{item.get('blocking') is True} |"
            )
    if sla_rows:
        lines.extend(
            [
                "",
                "## Capability SLA",
                "",
                "| task | item | actual | target | status | blocking |",
                "| --- | --- | ---: | ---: | --- | --- |",
                *sla_rows,
            ]
        )
    regression = payload.get("regression")
    if isinstance(regression, dict):
        lines.extend(["", "## Regression", ""])
        lines.extend(evaluation_regression_markdown(regression).splitlines()[2:])
    return "\n".join(lines) + "\n"


def evaluation_regression_markdown(payload: dict[str, Any]) -> str:
    summary = _dict(payload.get("summary") or {}, "regression.summary")
    lines = [
        f"# Agent Evaluation Regression `{payload.get('baseline_run_id', '')}` -> `{payload.get('candidate_run_id', '')}`",
        "",
        f"- evaluation passed rate delta: {summary.get('evaluation_passed_rate_delta', 0):.4f}",
        f"- verification pass rate delta: {summary.get('verification_pass_rate_delta', 0):.4f}",
        f"- average turns delta: {summary.get('average_turns_delta', 0):.4f}",
        f"- average tool calls delta: {summary.get('average_tool_calls_delta', 0):.4f}",
        f"- policy blocks delta: {summary.get('policy_blocks_delta', 0)}",
        f"- miscompletion delta: {summary.get('miscompletion_delta', 0)}",
        f"- regression count: {summary.get('regression_count', 0)}",
        "",
        "| task | baseline | candidate | turn delta | tool delta | trace artifacts |",
        "| --- | --- | --- | ---: | ---: | --- |",
    ]
    for diff in payload.get("task_diffs") or []:
        if not isinstance(diff, dict):
            continue
        artifacts = ", ".join(str(item) for item in diff.get("trace_artifact_refs") or []) or "-"
        lines.append(
            "| "
            f"`{diff.get('task_id', '')}` | "
            f"{diff.get('baseline_status', '')} | "
            f"{diff.get('candidate_status', '')} | "
            f"{diff.get('turn_delta', 0)} | "
            f"{diff.get('tool_call_delta', 0)} | "
            f"{artifacts} |"
        )
    return "\n".join(lines) + "\n"


def _score_status(*, task_count: int, scored_task_count: int, infrastructure_blocked_count: int) -> str:
    if scored_task_count > 0:
        return "scored"
    if task_count > 0 and infrastructure_blocked_count == task_count:
        return "environment_blocker"
    return "empty"


def _evaluation_passed_from_payload(payload: dict[str, Any]) -> bool:
    return bool(payload.get("evaluation_passed"))


def _payload_tool_calls(payload: dict[str, Any]) -> int:
    return _safe_int(payload.get("tool_calls"))


def _summarize_capability_sla(results: list[EvaluationTaskResult]) -> dict[str, Any]:
    items: dict[str, dict[str, Any]] = {}
    violations: dict[str, int] = {}
    unavailable: dict[str, int] = {}
    for result in results:
        sla = result.capability_sla or {}
        if not isinstance(sla, dict):
            continue
        for name, item in (sla.get("items") or {}).items():
            if not isinstance(item, dict):
                continue
            items[str(name)] = _merge_sla_item(items.get(str(name)), item)
            status = str(item.get("status") or "")
            if status == "over_sla":
                violations[str(name)] = violations.get(str(name), 0) + 1
            elif status == "unavailable":
                unavailable[str(name)] = unavailable.get(str(name), 0) + 1
    return {
        "schema_version": "evaluation.capability_sla_summary/v1",
        "status": "over_sla" if violations else "unavailable" if unavailable else "within_sla",
        "blocking": False,
        "task_count": len(results),
        "violations": dict(sorted(violations.items())),
        "unavailable": dict(sorted(unavailable.items())),
        "items": items,
    }


def _merge_sla_item(previous: dict[str, Any] | None, current: dict[str, Any]) -> dict[str, Any]:
    if previous is None:
        return dict(current)
    if previous.get("status") != "over_sla" and current.get("status") == "over_sla":
        return dict(current)
    if previous.get("status") == "unavailable" and current.get("status") != "unavailable":
        return dict(current)
    previous_delta = _safe_float(previous.get("delta_seconds"))
    current_delta = _safe_float(current.get("delta_seconds"))
    if current_delta > previous_delta:
        return dict(current)
    previous_count_delta = _safe_int(previous.get("delta_count"))
    current_count_delta = _safe_int(current.get("delta_count"))
    if current_count_delta > previous_count_delta:
        return dict(current)
    return previous


def _safe_int(value: Any) -> int:
    try:
        return int(value or 0)
    except (TypeError, ValueError):
        return 0


def _safe_str(value: Any) -> str:
    return "" if value is None else str(value)


def _safe_float(value: Any) -> float:
    try:
        return float(value or 0.0)
    except (TypeError, ValueError):
        return 0.0


def _format_optional_float(value: Any) -> str:
    if value is None:
        return "unknown"
    return f"{_safe_float(value):.6f}"


def _format_optional_rate(value: Any) -> str:
    if value is None:
        return "unknown"
    return f"{_safe_float(value):.4f}"


def _float_map(value: dict[str, Any]) -> dict[str, float]:
    return {str(key): _safe_float(val) for key, val in value.items()}


def _average_rate(rates: dict[str, float]) -> float:
    if not rates:
        return 0.0
    return round(sum(rates.values()) / len(rates), 4)


def _rate(numerator: int, denominator: int) -> float:
    if denominator <= 0:
        return 0.0
    return round(numerator / denominator, 4)


def _float_delta(current: dict[str, Any], previous: dict[str, Any], key: str) -> float:
    return round(float(current.get(key, 0.0) or 0.0) - float(previous.get(key, 0.0) or 0.0), 6)


def _dict(value: Any, field_name: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ValueError(f"evaluation {field_name} must be an object.")
    return dict(value)
