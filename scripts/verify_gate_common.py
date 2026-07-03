#!/usr/bin/env python3
"""Shared helpers for local verification gate scripts."""

from __future__ import annotations

import json
import statistics
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any


@dataclass(frozen=True)
class CommandSummary:
    name: str
    command: list[str]
    duration_seconds: float
    exit_code: int
    skipped: bool = False
    skipped_reason: str = ""

    @property
    def passed(self) -> bool:
        return self.skipped or self.exit_code == 0

    def to_dict(self) -> dict[str, Any]:
        return {
            "name": self.name,
            "command": self.command,
            "duration_seconds": self.duration_seconds,
            "exit_code": self.exit_code,
            "passed": self.passed,
            "skipped": self.skipped,
            "skipped_reason": self.skipped_reason,
        }


def run_command(name: str, command: list[str], *, cwd: Path) -> CommandSummary:
    started = time.perf_counter()
    completed = subprocess.run(command, cwd=cwd, check=False)
    return CommandSummary(
        name=name,
        command=command,
        duration_seconds=round(time.perf_counter() - started, 3),
        exit_code=completed.returncode,
    )


def skipped_command(name: str, reason: str) -> CommandSummary:
    return CommandSummary(
        name=name,
        command=[],
        duration_seconds=0.0,
        exit_code=0,
        skipped=True,
        skipped_reason=reason,
    )


def changed_python_scopes(changed_files: list[str]) -> list[str]:
    scopes: list[str] = []
    for raw_path in changed_files:
        path = Path(raw_path)
        if path.suffix != ".py":
            continue
        if not (path.exists() and (path.parts[:1] in {("src",), ("scripts",)} or path.parts[:2] == ("tests", "evaluation"))):
            continue
        if path.parts[:1] == ("src",):
            scopes.append(str(Path("src")))
        elif path.parts[:1] == ("scripts",):
            scopes.append(str(path))
    return sorted(dict.fromkeys(scopes))


def print_json_summary(summary: dict[str, Any]) -> None:
    print(json.dumps(summary, ensure_ascii=False, indent=2, sort_keys=True))


def repo_root_from_script(script_file: str) -> Path:
    return Path(script_file).resolve().parents[1]


def python() -> str:
    return sys.executable


def timing_summary(commands: list[CommandSummary], *, total_wall_time: float, extra: dict[str, Any] | None = None) -> dict[str, Any]:
    by_name = {command.name: command.duration_seconds for command in commands}
    summary: dict[str, Any] = {
        "total_wall_time_seconds": round(total_wall_time, 3),
        "ruff_time_seconds": by_name.get("ruff", 0.0),
        "mypy_time_seconds": by_name.get("mypy", 0.0),
        "compileall_time_seconds": by_name.get("compileall", by_name.get("compileall_changed_scope", 0.0)),
        "pytest_time_seconds": round(
            sum(command.duration_seconds for command in commands if "pytest" in command.name or command.name.endswith("_tests")),
            3,
        ),
        "runtime_docs_time_seconds": by_name.get("runtime_docs", 0.0),
        "capability_eval_time_seconds": by_name.get("public_representative_eval", 0.0),
        "provider_time_seconds": 0.0,
        "sandbox_time_seconds": 0.0,
        "verification_time_seconds": 0.0,
        "context_retrieval_compaction_time_seconds": 0.0,
        "selected_tests_count": 0,
        "skipped_tests_count": 0,
        "fallback_reason": "",
    }
    if extra:
        summary.update(extra)
    return summary


def capability_timing_from_result(result_path: Path) -> dict[str, Any]:
    if not result_path.exists():
        return {}
    try:
        payload = json.loads(result_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {}
    aggregated: dict[str, float | None] = {}
    diagnostics: dict[str, Any] = {}
    for task in payload.get("tasks") or []:
        if not isinstance(task, dict):
            continue
        capability = task.get("capability_summary") or {}
        if not isinstance(capability, dict):
            continue
        timing = task.get("timing") or capability.get("timing") or {}
        if isinstance(timing, dict):
            for name, value in timing.items():
                if isinstance(value, int | float):
                    aggregated[name] = round(float(aggregated.get(name) or 0.0) + float(value), 3)
                elif value is None and name not in aggregated:
                    aggregated[name] = None
        task_diagnostics = capability.get("timing_diagnostics") or {}
        if isinstance(task_diagnostics, dict):
            diagnostics.update(task_diagnostics)
    if diagnostics:
        aggregated["timing_diagnostics"] = diagnostics
    return aggregated


def capability_metrics_from_result(result_path: Path) -> dict[str, Any]:
    if not result_path.exists():
        return {}
    try:
        payload = json.loads(result_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {}
    summary = payload.get("summary") or {}
    if not isinstance(summary, dict):
        summary = {}
    cost_sources: dict[str, int] = {}
    pricing_statuses: dict[str, int] = {}
    task_metrics_count = 0
    for task in payload.get("tasks") or []:
        if not isinstance(task, dict):
            continue
        metrics = task.get("evaluation_metrics") or {}
        if not isinstance(metrics, dict):
            continue
        task_metrics_count += 1
        cost = metrics.get("cost") or {}
        if not isinstance(cost, dict):
            continue
        source = str(cost.get("cost_source") or "unknown")
        pricing_status = str(cost.get("pricing_status") or "unknown")
        cost_sources[source] = cost_sources.get(source, 0) + 1
        pricing_statuses[pricing_status] = pricing_statuses.get(pricing_status, 0) + 1
    return {
        "resolved_count": int(summary.get("resolved_count") or 0),
        "resolved_rate": float(summary.get("resolved_rate") or 0.0),
        "total_cost_estimate": summary.get("total_cost_estimate"),
        "cost_per_resolved": summary.get("cost_per_resolved"),
        "average_tool_success_rate": summary.get("average_tool_success_rate"),
        "cost_sources": dict(sorted(cost_sources.items())),
        "pricing_statuses": dict(sorted(pricing_statuses.items())),
        "task_metrics_count": task_metrics_count,
    }


def capability_sla_from_result(result_path: Path) -> dict[str, Any]:
    if not result_path.exists():
        return {}
    try:
        payload = json.loads(result_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {}
    summary = payload.get("summary") or {}
    if isinstance(summary, dict) and isinstance(summary.get("capability_sla"), dict):
        result = dict(summary["capability_sla"])
    else:
        result = {
            "schema_version": "evaluation.capability_sla_summary/v1",
            "status": "unknown",
            "blocking": False,
            "violations": {},
            "task_count": 0,
        }
    items: dict[str, dict[str, Any]] = {}
    for task in payload.get("tasks") or []:
        if not isinstance(task, dict):
            continue
        sla = task.get("capability_sla") or {}
        if not isinstance(sla, dict):
            continue
        for name, item in (sla.get("items") or {}).items():
            if isinstance(item, dict):
                items[str(name)] = dict(item)
    if items:
        result["items"] = dict(sorted(items.items()))
    return result


def capability_latency_attribution_from_result(result_path: Path) -> dict[str, Any]:
    if not result_path.exists():
        return {}
    try:
        payload = json.loads(result_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {}
    aggregated: dict[str, dict[str, Any]] = {}
    critical_path: list[dict[str, Any]] = []
    task_count = 0
    for task in payload.get("tasks") or []:
        if not isinstance(task, dict):
            continue
        task_id = str(task.get("task_id") or "")
        capability = task.get("capability_summary") or {}
        if not isinstance(capability, dict):
            continue
        attribution = capability.get("latency_attribution") or {}
        if isinstance(attribution, dict):
            items = attribution.get("items") if isinstance(attribution.get("items"), dict) else {}
            if items:
                task_count += 1
            for name, item in items.items():
                if not isinstance(item, dict):
                    continue
                component = str(item.get("component") or name)
                actual = item.get("actual_seconds")
                target = aggregated.setdefault(
                    component,
                    {
                        "component": component,
                        "actual_seconds": 0.0,
                        "source": str(item.get("source") or ""),
                        "kind": str(item.get("kind") or ""),
                        "critical_path": False,
                        "status": str(item.get("status") or ""),
                        "notes": str(item.get("notes") or ""),
                    },
                )
                if isinstance(actual, int | float):
                    target["actual_seconds"] = round(float(target["actual_seconds"]) + float(actual), 3)
                target["critical_path"] = bool(target.get("critical_path") or item.get("critical_path"))
                if not target["status"] and item.get("status"):
                    target["status"] = str(item.get("status") or "")
                if not target["notes"] and item.get("notes"):
                    target["notes"] = str(item.get("notes") or "")
        task_breakdown = capability.get("critical_path_breakdown") or []
        for item in task_breakdown:
            if not isinstance(item, dict):
                continue
            critical_path.append(
                {
                    "task_id": task_id,
                    "component": str(item.get("component") or ""),
                    "actual_seconds": _safe_optional_float(item.get("actual_seconds")),
                    "source": str(item.get("source") or ""),
                    "kind": str(item.get("kind") or ""),
                    "critical_path": bool(item.get("critical_path")),
                    "status": str(item.get("status") or ""),
                    "notes": str(item.get("notes") or ""),
                }
            )
    return {
        "schema_version": "evaluation.latency_attribution_summary/v1",
        "status": "available" if aggregated else "unavailable",
        "task_count": task_count,
        "items": dict(sorted(aggregated.items())),
        "critical_path_breakdown": sorted(
            critical_path,
            key=lambda item: float(item.get("actual_seconds") or 0.0),
            reverse=True,
        )[:12],
        "blocking": False,
    }


_SLA_ITEM_TO_TIMING_METRIC = {
    "wall": "wall_time_seconds",
    "agent_loop": "agent_loop_time_seconds",
    "provider": "provider_time_seconds",
    "sandbox": "sandbox_time_seconds",
    "dependency_setup": "dependency_setup_time_seconds",
    "verification": "verification_time_seconds",
    "unattributed_time": "unattributed_time_seconds",
}


def capability_sla_diagnostics(
    capability_sla: dict[str, Any],
    timing_compare: dict[str, Any],
) -> dict[str, Any]:
    items = capability_sla.get("items") if isinstance(capability_sla, dict) else {}
    compare_metrics = timing_compare.get("metrics") if isinstance(timing_compare, dict) else {}
    compare_available = (
        isinstance(timing_compare, dict)
        and timing_compare.get("status") == "available"
        and isinstance(compare_metrics, dict)
    )
    diagnostic_items: dict[str, dict[str, Any]] = {}
    if isinstance(items, dict):
        for name, item in items.items():
            if not isinstance(item, dict):
                continue
            metric_name = _SLA_ITEM_TO_TIMING_METRIC.get(str(name))
            if metric_name is None:
                continue
            comparison = compare_metrics.get(metric_name) if isinstance(compare_metrics, dict) else {}
            diagnostic_items[str(name)] = _sla_diagnostic_item(
                item,
                metric_name=metric_name,
                comparison=comparison if isinstance(comparison, dict) else {},
                compare_available=compare_available,
            )
    return {
        "schema_version": "evaluation.capability_sla_diagnostics/v1",
        "status": str(capability_sla.get("status") or "unknown") if isinstance(capability_sla, dict) else "unknown",
        "blocking": False,
        "compare_status": str(timing_compare.get("status") or "unavailable")
        if isinstance(timing_compare, dict)
        else "unavailable",
        "run_count": int(timing_compare.get("run_count") or 0) if isinstance(timing_compare, dict) else 0,
        "items": dict(sorted(diagnostic_items.items())),
    }


def _sla_diagnostic_item(
    item: dict[str, Any],
    *,
    metric_name: str,
    comparison: dict[str, Any],
    compare_available: bool,
) -> dict[str, Any]:
    status = str(item.get("status") or "")
    target = _safe_optional_float(item.get("target_seconds"))
    delta = _safe_optional_float(item.get("delta_seconds"))
    current = _safe_optional_float(comparison.get("current"))
    minimum = _safe_optional_float(comparison.get("min"))
    median = _safe_optional_float(comparison.get("median"))
    diagnosis = "within_sla"
    reason = ""
    if status == "over_sla":
        if not compare_available or target is None or median is None:
            diagnosis = "current_run_over_sla"
            reason = "repeated timing compare unavailable or incomplete"
        elif median > target:
            diagnosis = "persistent_over_sla"
            reason = "median repeated timing exceeds target"
        elif delta is not None and 0.0 < delta <= 1.0:
            diagnosis = "timing_jitter"
            reason = "current run is slightly over target while median repeated timing is within target"
        else:
            diagnosis = "current_run_over_sla"
            reason = "current run exceeds target but repeated median is within target"
    elif status == "unavailable":
        diagnosis = "unavailable"
        reason = "current SLA item is unavailable"
    return {
        "metric": metric_name,
        "status": status,
        "diagnosis": diagnosis,
        "reason": reason,
        "current_seconds": current,
        "min_seconds": minimum,
        "median_seconds": median,
        "target_seconds": target,
        "delta_seconds": delta,
        "blocking": False,
    }


def _safe_optional_float(value: Any) -> float | None:
    if isinstance(value, int | float):
        return round(float(value), 3)
    return None


def capability_repeated_timing_compare(result_path: Path) -> dict[str, Any]:
    current = _capability_timing_record(result_path)
    if not current:
        return {
            "schema_version": "evaluation.capability_timing_compare/v1",
            "status": "unavailable",
            "reason": "current result is unavailable or contains no task timing",
            "run_count": 0,
            "metrics": {},
        }
    output_root = result_path.parent.parent
    records: list[dict[str, Any]] = []
    for candidate in sorted(output_root.glob("*/result.json")):
        record = _capability_timing_record(candidate)
        if not record:
            continue
        if record.get("task_id") != current.get("task_id"):
            continue
        if record.get("start_commit") != current.get("start_commit"):
            continue
        records.append(record)
    metrics: dict[str, dict[str, float | None]] = {}
    metric_names = sorted({name for record in records for name in record.get("metrics", {})})
    for name in metric_names:
        current_value = current.get("metrics", {}).get(name)
        values = [
            float(value)
            for record in records
            for value in [record.get("metrics", {}).get(name)]
            if isinstance(value, int | float)
        ]
        if not values:
            continue
        metrics[name] = {
            "current": round(float(current_value), 3) if isinstance(current_value, int | float) else None,
            "min": round(min(values), 3),
            "median": round(float(statistics.median(values)), 3),
        }
    return {
        "schema_version": "evaluation.capability_timing_compare/v1",
        "status": "available" if records else "unavailable",
        "task_id": current.get("task_id"),
        "start_commit": current.get("start_commit"),
        "current_run_id": result_path.parent.name,
        "run_count": len(records),
        "metrics": metrics,
    }


def _capability_timing_record(result_path: Path) -> dict[str, Any]:
    if not result_path.exists():
        return {}
    try:
        payload = json.loads(result_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {}
    tasks = payload.get("tasks") or []
    task = next((item for item in tasks if isinstance(item, dict)), None)
    if task is None:
        return {}
    timing = task.get("timing") if isinstance(task.get("timing"), dict) else {}
    capability = task.get("capability_summary") if isinstance(task.get("capability_summary"), dict) else {}
    wall_phases = capability.get("wall_phases") if isinstance(capability.get("wall_phases"), dict) else {}
    workspace = {}
    environment = task.get("reproducible_environment")
    if isinstance(environment, dict) and isinstance(environment.get("workspace"), dict):
        workspace = environment["workspace"]
    metrics: dict[str, float] = {}
    for name in (
        "wall_time_seconds",
        "dependency_setup_time_seconds",
        "sandbox_time_seconds",
        "provider_time_seconds",
        "verification_time_seconds",
    ):
        value = timing.get(name)
        if isinstance(value, int | float):
            metrics[name] = round(float(value), 3)
    agent_loop = wall_phases.get("agent_loop_time_seconds")
    if isinstance(agent_loop, int | float):
        metrics["agent_loop_time_seconds"] = round(float(agent_loop), 3)
    unattributed = capability.get("unattributed_time_seconds")
    if isinstance(unattributed, int | float):
        metrics["unattributed_time_seconds"] = round(float(unattributed), 3)
    breakdown = capability.get("sandbox_breakdown") if isinstance(capability, dict) else {}
    if isinstance(breakdown, dict):
        items = breakdown.get("items") if isinstance(breakdown.get("items"), dict) else {}
        for name, item in items.items():
            if not isinstance(item, dict):
                continue
            value = item.get("actual_seconds")
            if isinstance(value, int | float):
                metrics[f"sandbox_breakdown.{name}.actual_seconds"] = round(float(value), 3)
    attribution = capability.get("latency_attribution") if isinstance(capability, dict) else {}
    if isinstance(attribution, dict):
        items = attribution.get("items") if isinstance(attribution.get("items"), dict) else {}
        for name, item in items.items():
            if not isinstance(item, dict):
                continue
            value = item.get("actual_seconds")
            if isinstance(value, int | float):
                metrics[f"latency_attribution.{name}.actual_seconds"] = round(float(value), 3)
    return {
        "task_id": str(task.get("task_id") or ""),
        "start_commit": str(workspace.get("start_commit") or ""),
        "run_id": result_path.parent.name,
        "metrics": metrics,
    }


def capability_turns_from_result(result_path: Path) -> dict[str, Any]:
    if not result_path.exists():
        return {}
    try:
        payload = json.loads(result_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {}
    turns: list[dict[str, Any]] = []
    for task in payload.get("tasks") or []:
        if not isinstance(task, dict):
            continue
        capability = task.get("capability_summary") or {}
        if not isinstance(capability, dict):
            continue
        for turn in capability.get("turn_diagnostics") or []:
            if isinstance(turn, dict):
                turns.append(turn)
    slowest = sorted(
        turns,
        key=lambda item: float(item.get("provider_duration_seconds") or 0.0),
        reverse=True,
    )[:5]
    return {
        "turn_count": len(turns),
        "provider_time_seconds": round(
            sum(float(turn.get("provider_duration_seconds") or 0.0) for turn in turns),
            3,
        ),
        "tool_call_count": sum(len(turn.get("tool_calls") or []) for turn in turns),
        "review_event_count": sum(len(turn.get("review_events") or []) for turn in turns),
        "denied_tool_count": sum(len(turn.get("denied_tools") or []) for turn in turns),
        "slowest_turns": [
            {
                "turn": turn.get("turn"),
                "phase_id": turn.get("phase_id"),
                "purpose": turn.get("purpose"),
                "provider_duration_seconds": turn.get("provider_duration_seconds"),
                "tool_call_count": len(turn.get("tool_calls") or []),
                "review_event_count": len(turn.get("review_events") or []),
                "denied_tool_count": len(turn.get("denied_tools") or []),
            }
            for turn in slowest
        ],
    }


def capability_policy_diagnostics_from_result(result_path: Path) -> dict[str, Any]:
    if not result_path.exists():
        return {}
    try:
        payload = json.loads(result_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {}
    denied_tools: list[dict[str, Any]] = []
    exposure_by_turn: list[dict[str, Any]] = []
    for task in payload.get("tasks") or []:
        if not isinstance(task, dict):
            continue
        task_id = str(task.get("task_id") or "")
        capability = task.get("capability_summary") or {}
        if not isinstance(capability, dict):
            continue
        for turn in capability.get("turn_diagnostics") or []:
            if not isinstance(turn, dict):
                continue
            turn_id = int(turn.get("turn") or 0)
            phase = str(turn.get("phase_id") or "")
            for item in turn.get("denied_tools") or []:
                if not isinstance(item, dict):
                    continue
                denied_tools.append(
                    {
                        "task_id": task_id,
                        "turn": turn_id,
                        "phase": phase,
                        "tool_name": str(item.get("tool_name") or ""),
                        "error_code": str(item.get("error_code") or ""),
                        "blocked_reason": str(item.get("blocked_reason") or ""),
                        "stage_basis": str(item.get("stage_basis") or ""),
                    }
                )
            exposure = turn.get("tool_exposure") if isinstance(turn.get("tool_exposure"), dict) else {}
            if exposure:
                exposure_by_turn.append(
                    {
                        "task_id": task_id,
                        "turn": turn_id,
                        "phase": phase,
                        "selected_tools": [str(item) for item in exposure.get("selected_tools") or []],
                        "blocked_tools": _exposure_names(exposure.get("blocked_tools")),
                        "deferred_tools": _exposure_names(exposure.get("deferred_tools")),
                        "suppressed_tools": _exposure_names(exposure.get("suppressed_tools")),
                    }
                )
    return {
        "schema_version": "evaluation.policy_diagnostics_summary/v1",
        "blocking": False,
        "denied_tool_count": len(denied_tools),
        "denied_tools": denied_tools,
        "tool_exposure_by_turn": exposure_by_turn,
    }


def _exposure_names(payload: Any) -> list[str]:
    names: list[str] = []
    for item in payload or []:
        if isinstance(item, dict):
            names.append(str(item.get("name") or ""))
        else:
            names.append(str(item))
    return names


def capability_review_from_result(result_path: Path) -> dict[str, Any]:
    if not result_path.exists():
        return {}
    try:
        payload = json.loads(result_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {}
    review_events: list[dict[str, Any]] = []
    review_time = 0.0
    critic_time = 0.0
    provider_latency_by_stage: dict[str, dict[str, Any]] = {}
    for task in payload.get("tasks") or []:
        if not isinstance(task, dict):
            continue
        task_id = str(task.get("task_id") or "")
        capability = task.get("capability_summary") or {}
        if not isinstance(capability, dict):
            continue
        _merge_review_stage_latency(
            provider_latency_by_stage,
            _safe_review_stage_latency(capability.get("provider_latency_by_review_stage")),
        )
        timing = capability.get("timing") or {}
        if isinstance(timing, dict):
            review_time += float(timing.get("edit_apply_review_time_seconds") or 0.0)
            critic_time += float(timing.get("edit_apply_critic_time_seconds") or 0.0)
        for turn in capability.get("turn_diagnostics") or []:
            if not isinstance(turn, dict):
                continue
            for event in turn.get("review_events") or []:
                if not isinstance(event, dict):
                    continue
                review_events.append(
                    {
                        "task_id": task_id,
                        "turn": turn.get("turn"),
                        "stage": event.get("stage"),
                        "duration_seconds": event.get("duration_seconds"),
                        "critic_duration_seconds": event.get("critic_duration_seconds"),
                        "model_critic_status": event.get("model_critic_status"),
                        "output_mode": str(event.get("output_mode") or ""),
                        "schema_validation_passed": bool(event.get("schema_validation_passed")),
                        "retry_count": int(event.get("retry_count") or 0),
                        "retry_reason": str(event.get("retry_reason") or "none"),
                        "fallback_reason": str(event.get("fallback_reason") or ""),
                        "critic_reused": bool(event.get("critic_reused")),
                        "critic_skipped_reason": str(event.get("critic_skipped_reason") or ""),
                        "critic_reuse_skip_reason": str(event.get("critic_reuse_skip_reason") or ""),
                        "critic_source_status": str(event.get("critic_source_status") or ""),
                    }
                )
    return {
        "edit_apply_review_time_seconds": round(review_time, 3),
        "edit_apply_critic_time_seconds": round(critic_time, 3),
        "review_event_count": len(review_events),
        "critic_reused_count": sum(1 for event in review_events if event.get("critic_reused")),
        "critic_skipped_count": sum(1 for event in review_events if event.get("critic_skipped_reason")),
        "review_events": review_events,
        "provider_latency_by_review_stage": _safe_review_stage_latency(provider_latency_by_stage),
    }


def _safe_review_stage_latency(payload: Any) -> dict[str, dict[str, Any]]:
    if not isinstance(payload, dict):
        return {}
    result: dict[str, dict[str, Any]] = {}
    for stage, item in payload.items():
        if not isinstance(item, dict):
            continue
        result[str(stage)] = {
            "call_count": int(item.get("call_count") or 0),
            "failed_call_count": int(item.get("failed_call_count") or 0),
            "total_seconds": round(float(item.get("total_seconds") or 0.0), 3),
            "max_seconds": round(float(item.get("max_seconds") or 0.0), 3),
        }
    return dict(sorted(result.items()))


def _merge_review_stage_latency(
    target: dict[str, dict[str, Any]],
    source: dict[str, dict[str, Any]],
) -> None:
    for stage, item in source.items():
        current = target.setdefault(
            stage,
            {
                "call_count": 0,
                "failed_call_count": 0,
                "total_seconds": 0.0,
                "max_seconds": 0.0,
            },
        )
        current["call_count"] = int(current.get("call_count") or 0) + int(item.get("call_count") or 0)
        current["failed_call_count"] = int(current.get("failed_call_count") or 0) + int(
            item.get("failed_call_count") or 0
        )
        current["total_seconds"] = round(
            float(current.get("total_seconds") or 0.0) + float(item.get("total_seconds") or 0.0),
            3,
        )
        current["max_seconds"] = max(
            float(current.get("max_seconds") or 0.0),
            float(item.get("max_seconds") or 0.0),
        )
