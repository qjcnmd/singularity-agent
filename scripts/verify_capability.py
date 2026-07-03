#!/usr/bin/env python3
"""Capability gate for the single public representative real task."""

from __future__ import annotations

import argparse
import json
import subprocess
import time
from pathlib import Path
from typing import Any

from verify_gate_common import (
    capability_latency_attribution_from_result,
    capability_metrics_from_result,
    capability_policy_diagnostics_from_result,
    capability_repeated_timing_compare,
    capability_review_from_result,
    capability_sla_diagnostics,
    capability_sla_from_result,
    capability_timing_from_result,
    capability_turns_from_result,
    print_json_summary,
    python,
    repo_root_from_script,
    run_command,
    timing_summary,
)

DEFAULT_MANIFEST = "docs/evaluation/public-representative-task.json"
DEFAULT_RUN_ID = "public-long-task-gate"


def _impact(files: list[str], *, cwd: Path) -> dict[str, Any]:
    command = [python(), "scripts/test_impact.py", "--json"]
    if files:
        command.extend(files)
    else:
        command.append("--git")
    completed = subprocess.run(command, cwd=cwd, text=True, capture_output=True, check=False)
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError:
        return {
            "changed_files": files,
            "warnings": [completed.stderr.strip() or completed.stdout.strip() or "test impact failed"],
            "capability_gate": {"required": False, "areas": [], "files": [], "trigger": ""},
        }


def main() -> int:
    parser = argparse.ArgumentParser(description="Run the public representative capability gate.")
    parser.add_argument("files", nargs="*", help="Changed files for trigger detection.")
    parser.add_argument("--force", action="store_true", help="Run even when no core-chain trigger is detected.")
    parser.add_argument("--manifest", default=DEFAULT_MANIFEST, help="Evaluation manifest path.")
    parser.add_argument("--run-id", default=DEFAULT_RUN_ID, help="Evaluation run id.")
    args = parser.parse_args()

    cwd = repo_root_from_script(__file__)
    started = time.perf_counter()
    impact = _impact(args.files, cwd=cwd)
    capability_gate = impact.get("capability_gate") or {}
    if not args.force and not capability_gate.get("required"):
        duration = round(time.perf_counter() - started, 3)
        print_json_summary(
            {
                "gate": "capability",
                "passed": True,
                "ran": False,
                "skipped_reason": "no AgentLoop/ToolProtocol/sandbox/context/compaction/verification/CompletionGate/FinalReport/evaluation runner changes detected",
                "impact": impact,
                "commands": [],
                "duration_seconds": duration,
                "timing": timing_summary(
                    [],
                    total_wall_time=duration,
                    extra={
                        "selected_tests_count": 0,
                        "skipped_tests_count": 1,
                        "fallback_reason": "capability gate not triggered",
                    },
                ),
            }
        )
        return 0

    command = [
        python(),
        "-m",
        "singularity.cli",
        "eval",
        "run",
        args.manifest,
        "--run-id",
        args.run_id,
        "--json",
    ]
    result = run_command("public_representative_eval", command, cwd=cwd)
    duration = round(time.perf_counter() - started, 3)
    result_path = cwd / "work" / "evaluations" / args.run_id / "result.json"
    capability_timing = capability_timing_from_result(result_path)
    capability_metrics = capability_metrics_from_result(result_path)
    capability_sla = capability_sla_from_result(result_path)
    latency_attribution = capability_latency_attribution_from_result(result_path)
    timing_compare = capability_repeated_timing_compare(result_path)
    sla_diagnostics = capability_sla_diagnostics(capability_sla, timing_compare)
    turn_diagnostics = capability_turns_from_result(result_path)
    review_diagnostics = capability_review_from_result(result_path)
    policy_diagnostics = capability_policy_diagnostics_from_result(result_path)
    phase_8_12_vs_current_timing = _phase_8_12_vs_current_timing(result_path, cwd=cwd)
    print_json_summary(
        {
            "gate": "capability",
            "passed": result.passed,
            "ran": True,
            "manifest": args.manifest,
            "run_id": args.run_id,
            "impact": impact,
            "commands": [result.to_dict()],
            "duration_seconds": duration,
            "result_path": str(result_path),
            "evaluation_metrics": capability_metrics,
            "capability_sla": capability_sla,
            "latency_attribution": latency_attribution,
            "critical_path_breakdown": latency_attribution.get("critical_path_breakdown", []),
            "capability_sla_diagnostics": sla_diagnostics,
            "policy_diagnostics": policy_diagnostics,
            "8_12_vs_current_timing": phase_8_12_vs_current_timing,
            "timing_compare": timing_compare,
            "turn_diagnostics": turn_diagnostics,
            "review_diagnostics": review_diagnostics,
            "remaining_bottlenecks": _remaining_bottlenecks(capability_sla, sla_diagnostics),
            "timing": timing_summary(
                [result],
                total_wall_time=duration,
                extra={
                    **capability_timing,
                    "selected_tests_count": 1,
                    "skipped_tests_count": 0,
                    "fallback_reason": "",
                },
            ),
        }
    )
    return 0 if result.passed else 1


def _phase_8_12_vs_current_timing(result_path: Path, *, cwd: Path) -> dict[str, Any]:
    baseline_run_id = "phase8-12-agentloop-latency-attribution-rerun"
    baseline_path = cwd / "work" / "evaluations" / baseline_run_id / "result.json"
    baseline = _timing_record_for_compare(baseline_path)
    current = _timing_record_for_compare(result_path)
    if not baseline or not current:
        return {
            "schema_version": "evaluation.phase_timing_compare/v1",
            "status": "unavailable",
            "baseline_run_id": baseline_run_id,
            "current_run_id": result_path.parent.name,
            "reason": "baseline or current result is unavailable",
            "metrics": {},
        }
    metrics: dict[str, dict[str, float | None]] = {}
    for name in sorted(set(baseline) | set(current)):
        baseline_value = baseline.get(name)
        current_value = current.get(name)
        delta = (
            round(float(current_value) - float(baseline_value), 3)
            if isinstance(baseline_value, int | float) and isinstance(current_value, int | float)
            else None
        )
        metrics[name] = {
            "phase_8_12_seconds": baseline_value,
            "current_seconds": current_value,
            "delta_seconds": delta,
            "status": "improved" if isinstance(delta, int | float) and delta < 0 else "regressed"
            if isinstance(delta, int | float) and delta > 0
            else "unchanged"
            if delta == 0
            else "unavailable",
        }
    return {
        "schema_version": "evaluation.phase_timing_compare/v1",
        "status": "available",
        "baseline_run_id": baseline_run_id,
        "current_run_id": result_path.parent.name,
        "metrics": metrics,
    }


def _timing_record_for_compare(result_path: Path) -> dict[str, float]:
    if not result_path.exists():
        return {}
    try:
        payload = json.loads(result_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {}
    task = next((item for item in payload.get("tasks") or [] if isinstance(item, dict)), None)
    if task is None:
        return {}
    timing = task.get("timing") if isinstance(task.get("timing"), dict) else {}
    capability = task.get("capability_summary") if isinstance(task.get("capability_summary"), dict) else {}
    wall_phases = capability.get("wall_phases") if isinstance(capability.get("wall_phases"), dict) else {}
    metrics: dict[str, float] = {}
    for output_name, source in (
        ("wall", timing.get("wall_time_seconds")),
        ("provider", timing.get("provider_time_seconds")),
        ("sandbox", timing.get("sandbox_time_seconds")),
        ("verification", timing.get("verification_time_seconds")),
        ("dependency_setup", timing.get("dependency_setup_time_seconds")),
        ("agent_loop", wall_phases.get("agent_loop_time_seconds")),
        ("unattributed_time", capability.get("unattributed_time_seconds")),
    ):
        if isinstance(source, int | float):
            metrics[output_name] = round(float(source), 3)
    return metrics


def _remaining_bottlenecks(
    capability_sla: dict[str, Any],
    sla_diagnostics: dict[str, Any] | None = None,
) -> list[dict[str, Any]]:
    items = capability_sla.get("items") if isinstance(capability_sla, dict) else {}
    if not isinstance(items, dict):
        return []
    diagnostic_items = (
        sla_diagnostics.get("items")
        if isinstance(sla_diagnostics, dict) and isinstance(sla_diagnostics.get("items"), dict)
        else {}
    )
    rows: list[dict[str, Any]] = []
    for name, item in items.items():
        if not isinstance(item, dict):
            continue
        delta = item.get("delta_seconds")
        if not isinstance(delta, int | float) or float(delta) <= 0:
            continue
        rows.append(
            {
                "name": str(name),
                "status": item.get("status"),
                "actual_seconds": item.get("actual_seconds"),
                "target_seconds": item.get("target_seconds"),
                "delta_seconds": round(float(delta), 3),
                "diagnosis": (diagnostic_items.get(name) or {}).get("diagnosis")
                if isinstance(diagnostic_items, dict)
                else "",
                "current_seconds": (diagnostic_items.get(name) or {}).get("current_seconds")
                if isinstance(diagnostic_items, dict)
                else None,
                "min_seconds": (diagnostic_items.get(name) or {}).get("min_seconds")
                if isinstance(diagnostic_items, dict)
                else None,
                "median_seconds": (diagnostic_items.get(name) or {}).get("median_seconds")
                if isinstance(diagnostic_items, dict)
                else None,
            }
        )
    return sorted(rows, key=lambda item: float(item["delta_seconds"]), reverse=True)


if __name__ == "__main__":
    raise SystemExit(main())
